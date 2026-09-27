use super::*;
use base64::Engine;
use prost::Message as _;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use wire::Message as P;

pub(super) type Blobs = HashMap<Vec<u8>, Vec<u8>>;
fn store(blobs: &mut Blobs, data: Vec<u8>) -> Vec<u8> {
    let id = Sha256::digest(&data).to_vec();
    blobs.insert(id.clone(), data);
    id
}
pub(super) fn image(data: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| ModelError::Request("Cursor image is not valid base64".into()))
}
pub(super) fn build(
    model: &str,
    context: &Context,
    tools: &[ToolSchema],
) -> Result<(Vec<u8>, Blobs)> {
    let mut blobs = Blobs::new();
    let mut roots = Vec::new();
    let messages = context.messages();
    let (history, text, images) = match messages.last() {
        Some(orca_harness_core::Message::User { content, images }) => (
            &messages[..messages.len() - 1],
            content.as_str(),
            images.as_slice(),
        ),
        _ => (messages, "Continue from the conversation above.", &[][..]),
    };
    for message in history {
        use orca_harness_core::Message::*;
        let root = match message {
            System { content } => {
                json!({"role":"user","content":[{"type":"text","text":format!("<rules>\n{content}\n</rules>")}]})
            }
            User { content, images } => {
                if !images.is_empty() {
                    return Err(ModelError::Request(
                        "Cursor replay of historical user images is unsupported".into(),
                    ));
                }
                json!({"role":"user","content":[{"type":"text","text":format!("<user_query>\n{content}\n</user_query>")}]})
            }
            Assistant {
                content,
                tool_calls,
            } => {
                let mut parts = Vec::new();
                if let Some(text) = content {
                    parts.push(json!({"type":"text","text":text}));
                }
                for call in tool_calls {
                    parts.push(json!({"type":"tool-call","toolCallId":call.id,"toolName":format!("mcp_orca_{}",call.name),"args":call.arguments}));
                }
                json!({"role":"assistant","content":parts})
            }
            Tool { results } => {
                json!({"role":"tool","content":results.iter().map(|r| json!({"type":"tool-result","toolCallId":r.call_id,"toolName":format!("mcp_orca_{}",r.tool_name),"result":r.output,"isError":r.is_error})).collect::<Vec<_>>()})
            }
        };
        roots.push(store(
            &mut blobs,
            serde_json::to_vec(&root).map_err(|e| invalid(e.to_string()))?,
        ));
    }
    let mut state = P::default().number(10, 1).bytes(22, "orca");
    let mut selected = P::default().bytes(22, "orca");
    for id in &roots {
        state = state.bytes(1, id);
        selected = selected.bytes(1, id);
    }
    let selected_id = store(&mut blobs, selected.0);
    let mut image_context = P::default();
    for img in images {
        image_context = image_context.bytes(
            1,
            P::default()
                .bytes(2, uuid::Uuid::new_v4().to_string())
                .bytes(7, &img.media_type)
                .bytes(8, image(&img.data)?)
                .0,
        );
    }
    let id = uuid::Uuid::new_v4().to_string();
    let user = P::default()
        .bytes(1, text)
        .bytes(2, &id)
        .bytes(3, image_context.0)
        .number(4, 1)
        .bytes(10, selected_id)
        .bytes(17, id);
    let action = P::default().bytes(1, P::default().bytes(1, user.0).0);
    let mut definitions = P::default();
    for tool in tools {
        definitions = definitions.bytes(
            1,
            P::default()
                .bytes(1, &tool.name)
                .bytes(2, &tool.description)
                .bytes(3, to_proto(&tool.parameters).encode_to_vec())
                .bytes(4, "orca")
                .bytes(5, &tool.name)
                .0,
        );
    }
    let run = P::default()
        .bytes(1, state.0)
        .bytes(2, action.0)
        .bytes(4, definitions.0)
        .bytes(5, uuid::Uuid::new_v4().to_string())
        .bytes(9, P::default().bytes(1, model).0);
    Ok((P::default().bytes(1, run.0).0, blobs))
}
pub(super) fn to_proto(v: &Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match v {
        Value::Null => Kind::NullValue(0),
        Value::Bool(b) => Kind::BoolValue(*b),
        Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or_default()),
        Value::String(s) => Kind::StringValue(s.clone()),
        Value::Array(a) => Kind::ListValue(prost_types::ListValue {
            values: a.iter().map(to_proto).collect(),
        }),
        Value::Object(o) => Kind::StructValue(prost_types::Struct {
            fields: o.iter().map(|(k, v)| (k.clone(), to_proto(v))).collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}
pub(super) fn from_proto(v: prost_types::Value) -> Value {
    use prost_types::value::Kind;
    match v.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(b)) => b.into(),
        Some(Kind::NumberValue(n)) => json!(n),
        Some(Kind::StringValue(s)) => s.into(),
        Some(Kind::ListValue(a)) => Value::Array(a.values.into_iter().map(from_proto).collect()),
        Some(Kind::StructValue(o)) => Value::Object(
            o.fields
                .into_iter()
                .map(|(k, v)| (k, from_proto(v)))
                .collect(),
        ),
    }
}

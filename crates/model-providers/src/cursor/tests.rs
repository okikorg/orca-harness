use super::*;
#[test]
fn frame_fragmentation_and_coalescing() {
    let a = wire::frame(&[10, 1, 65]).unwrap();
    let mut d = wire::Decoder::default();
    for byte in &a[..a.len() - 1] {
        d.push(&[*byte]);
        assert!(d.next().unwrap().is_none());
    }
    d.push(&[a[a.len() - 1]]);
    d.push(&a);
    assert_eq!(d.next().unwrap().unwrap().1, vec![10, 1, 65]);
    assert_eq!(d.next().unwrap().unwrap().1, vec![10, 1, 65]);
    assert!(d.next().unwrap().is_none());
}
#[test]
fn rejects_bad_wire_and_flags() {
    for bytes in [
        &[0][..],
        &[10, 4, 1][..],
        &[8, 255, 255, 255, 255, 255, 255, 255, 255, 255, 2][..],
    ] {
        assert!(Fields::parse(bytes).is_err());
    }
    for header in [[1, 0, 0, 0, 0], [0, 255, 255, 255, 255]] {
        let mut d = wire::Decoder::default();
        d.push(&header);
        assert!(d.next().is_err());
    }
    let encoded = P::default().bytes(1000, "future").number(1, 42).0;
    assert_eq!(Fields::parse(&encoded).unwrap().number(1), 42);
}
#[test]
fn request_has_hashed_history_images_and_protobuf_schema() {
    let mut ctx = Context::new();
    ctx.push_system("rules");
    ctx.push_user("old");
    ctx.push_user_with_images(
        "now",
        vec![orca_harness_core::Image {
            media_type: "image/png".into(),
            data: "AQID".into(),
        }],
    );
    let tools = vec![ToolSchema {
        name: "read".into(),
        description: "Read".into(),
        parameters: serde_json::json!({"type":"object"}),
    }];
    let (bytes, blobs) = request::build("auto", &ctx, &tools).unwrap();
    let outer = Fields::parse(&bytes).unwrap();
    let run = Fields::parse(outer.bytes(1)).unwrap();
    assert_eq!(
        Fields::parse(run.bytes(9)).unwrap().text(1).unwrap(),
        "auto"
    );
    let state = Fields::parse(run.bytes(1)).unwrap();
    assert_eq!(state.all(1).count(), 2);
    for id in state.all(1) {
        assert!(blobs.contains_key(id));
        assert_eq!(id.len(), 32);
    }
    let action = Fields::parse(run.bytes(2)).unwrap();
    let action = Fields::parse(action.bytes(1)).unwrap();
    let user = Fields::parse(action.bytes(1)).unwrap();
    assert_eq!(user.text(1).unwrap(), "now");
    assert!(blobs.contains_key(user.bytes(10)));
    let selected = Fields::parse(user.bytes(3)).unwrap();
    assert_eq!(
        Fields::parse(selected.bytes(1)).unwrap().bytes(8),
        [1, 2, 3]
    );
    let definitions = Fields::parse(run.bytes(4)).unwrap();
    let tool = Fields::parse(definitions.bytes(1)).unwrap();
    let schema = prost_types::Value::decode(tool.bytes(3)).unwrap();
    assert_eq!(request::from_proto(schema), tools[0].parameters);
}
#[test]
fn errors_and_http_contract() {
    assert!(matches!(
        rpc(CURSOR_BASE_URL, "", "Run", true),
        Err(ModelError::Authentication(_))
    ));
    let req = rpc("https://cursor.test/", "secret", "Run", true)
        .unwrap()
        .build()
        .unwrap();
    assert_eq!(req.version(), reqwest::Version::HTTP_2);
    assert_eq!(req.url().path(), "/agent.v1.AgentService/Run");
    assert_eq!(req.headers()["content-type"], "application/connect+proto");
    assert_eq!(req.headers()["authorization"], "Bearer secret");
    assert!(end_stream(b"{}").is_ok());
    assert!(end_stream(b"[]").is_err());
    assert!(matches!(
        end_stream(br#"{"error":{"code":"unauthenticated"}}"#),
        Err(ModelError::Authentication(_))
    ));
    assert!(end_stream(br#"{"error":{"code":"internal"}}"#).is_err());
}
#[test]
fn json_values_round_trip() {
    let value = serde_json::json!({"a":[null,true,2.5,"x"],"nested":{"b":false}});
    assert_eq!(request::from_proto(request::to_proto(&value)), value);
}

fn session(
    frames: Vec<Vec<u8>>,
) -> (
    Session,
    mpsc::UnboundedReceiver<std::result::Result<Vec<u8>, std::io::Error>>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    (
        Session {
            tx,
            stream: Box::pin(futures_util::stream::iter(frames.into_iter().map(Ok))),
            decoder: Default::default(),
            blobs: HashMap::new(),
            tools: vec!["read".into()],
            exec: Some((7, "exec-7".into())),
            heartbeat: tokio::spawn(std::future::pending()),
            created: std::time::Instant::now(),
        },
        rx,
    )
}
#[tokio::test]
async fn continuation_streams_text_and_usage_without_new_request() {
    let text = P::default()
        .bytes(1, P::default().bytes(1, P::default().bytes(1, "done").0).0)
        .0;
    let tokens = P::default()
        .bytes(1, P::default().bytes(8, P::default().number(1, 12).0).0)
        .0;
    let end = P::default().bytes(1, P::default().bytes(14, []).0).0;
    let (session, mut rx) = session(vec![
        wire::frame(&text).unwrap(),
        wire::frame(&tokens).unwrap(),
        wire::frame(&end).unwrap(),
    ]);
    let model = CursorModel::new("auto");
    model
        .pending
        .lock()
        .await
        .insert("cursor:local".into(), session);
    let mut context = Context::new();
    context.append_tool_results(vec![orca_harness_core::ToolResult { call_id:"cursor:local".into(),tool_name:"read".into(),output:serde_json::json!({"content":"ok","_images":[{"media_type":"image/png","data":"AQID"}]}),is_error:false }]);
    let response = model.generate(&context, &[]).await.unwrap();
    match response {
        ModelResponse::Final { text, usage } => {
            assert_eq!(text, "done");
            assert_eq!(usage.unwrap().output_tokens, 12);
        }
        _ => panic!("expected final"),
    }
    let packet = rx.recv().await.unwrap().unwrap();
    let mut decoder = wire::Decoder::default();
    decoder.push(&packet);
    let (_, payload) = decoder.next().unwrap().unwrap();
    let outer = Fields::parse(&payload).unwrap();
    let exec = Fields::parse(outer.bytes(2)).unwrap();
    assert_eq!(exec.number(1), 7);
    assert_eq!(exec.text(15).unwrap(), "exec-7");
    let result = Fields::parse(exec.bytes(11)).unwrap();
    assert_eq!(Fields::parse(result.bytes(1)).unwrap().all(1).count(), 2);
    assert!(model.pending.lock().await.is_empty());
}
#[tokio::test]
async fn premature_eof_is_not_a_success() {
    let (mut session, _) = session(vec![vec![0, 0]]);
    assert!(matches!(
        session.next().await,
        Err(ModelError::IncompleteResponse { .. })
    ));
}
#[tokio::test]
async fn kv_get_and_set_are_native_replies() {
    let (mut session, mut rx) = session(vec![]);
    let set = P::default()
        .number(1, 9)
        .bytes(3, P::default().bytes(1, "id").bytes(2, "blob").0);
    session.kv(&set.0).unwrap();
    rx.recv().await.unwrap().unwrap();
    session
        .kv(&P::default()
            .number(1, 10)
            .bytes(2, P::default().bytes(1, "id").0)
            .0)
        .unwrap();
    let packet = rx.recv().await.unwrap().unwrap();
    let mut decoder = wire::Decoder::default();
    decoder.push(&packet);
    let payload = decoder.next().unwrap().unwrap().1;
    let outer = Fields::parse(&payload).unwrap();
    let reply = Fields::parse(outer.bytes(3)).unwrap();
    assert_eq!(reply.number(1), 10);
    assert_eq!(Fields::parse(reply.bytes(2)).unwrap().bytes(1), b"blob");
}

#[tokio::test]
async fn native_mcp_call_maps_arguments_and_parks_connection() {
    let arg = request::to_proto(&serde_json::json!([true, 2.0])).encode_to_vec();
    let args = P::default()
        .bytes(5, "read")
        .bytes(2, P::default().bytes(1, "items").bytes(2, arg).0);
    let exec = P::default()
        .number(1, 22)
        .bytes(15, "remote-exec")
        .bytes(11, args.0);
    let (session, _rx) = session(vec![wire::frame(&P::default().bytes(2, exec.0).0).unwrap()]);
    let model = CursorModel::new("auto");
    model
        .pending
        .lock()
        .await
        .insert("cursor:previous".into(), session);
    let mut ctx = Context::new();
    ctx.append_tool_results(vec![orca_harness_core::ToolResult {
        call_id: "cursor:previous".into(),
        tool_name: "read".into(),
        output: "ok".into(),
        is_error: false,
    }]);
    match model.generate(&ctx, &[]).await.unwrap() {
        ModelResponse::ToolCalls { calls, .. } => {
            assert_eq!(calls[0].name, "read");
            assert_eq!(calls[0].arguments, serde_json::json!({"items":[true,2.0]}));
            assert!(model.pending.lock().await.contains_key(&calls[0].id));
        }
        _ => panic!("expected tool call"),
    }
}

use super::*;

#[test]
fn search_schema_and_runtime_share_a_ten_result_default() {
    let schema = SearchTools(McpCatalog::new()).schema();
    assert_eq!(SEARCH_DEFAULT_LIMIT, 10);
    assert_eq!(schema.parameters["properties"]["limit"]["default"], 10);
    assert_eq!(schema.parameters["properties"]["limit"]["maximum"], 100);
}

#[test]
fn search_normalizes_separators_and_basic_plurals() {
    assert_eq!(
        tokens("search_code-by-tag"),
        ["search", "code", "by", "tag"]
    );
    assert_eq!(singular("repositories"), "repository");
    assert_eq!(singular("branches"), "branch");
    assert_eq!(singular("tags"), "tag");
}

#[test]
fn search_ranks_exact_name_terms_above_description_and_plural_fallbacks() {
    let terms = tokens("search repositories");
    let exact = SearchFields {
        name: tokens("search_repositories"),
        description: vec![],
        server: tokens("github"),
        parameters: vec![],
    };
    let description_only = SearchFields {
        name: tokens("get_file"),
        description: tokens("search a repository"),
        server: tokens("github"),
        parameters: vec![],
    };
    assert!(relevance(&terms, &exact) > relevance(&terms, &description_only));
    assert!(relevance(&tokens("repository"), &exact).is_some());
}

#[test]
fn search_falls_back_to_partial_matches_for_natural_language_queries() {
    let terms = tokens("read source code project files");
    let read_file = SearchFields {
        name: tokens("read_project_file"),
        description: tokens("Read a UTF-8 project file within the configured project root"),
        server: tokens("plugin__explore_project__explore_project"),
        parameters: tokens("path max_chars"),
    };
    let tree = SearchFields {
        name: tokens("project_tree"),
        description: tokens("List project files and directories"),
        server: tokens("plugin__explore_project__explore_project"),
        parameters: tokens("path depth"),
    };

    assert_eq!(relevance(&terms, &read_file), None);
    assert!(partial_relevance(&terms, &read_file) > partial_relevance(&terms, &tree));
    assert_eq!(
        partial_relevance(&tokens("unrelated network request"), &read_file),
        None
    );
}

#[test]
fn interface_inputs_reject_invalid_optional_values_and_irrelevant_fields() {
    assert!(optional_u64(&json!({ "limit": 10 }), "limit", 1, 100).is_ok());
    for value in [json!(0), json!(101), json!(1.5), json!("10")] {
        assert!(optional_u64(&json!({ "limit": value }), "limit", 1, 100).is_err());
    }
    assert!(feature_request(
        "prompt_get",
        &json!({ "name": "review", "arguments": { "pr": 7 } })
    )
    .is_err());
    assert!(validate_feature_fields(
        "resource_read",
        &json!({ "action": "resource_read", "server": "s", "uri": "x", "name": "extra" })
    )
    .is_err());
    assert!(feature_request(
        "prompt_complete",
        &json!({ "name": "review", "argument": { "name": "pr", "value": "7", "extra": true } })
    )
    .is_err());
}

#[test]
fn feature_actions_require_their_advertised_capabilities() {
    let resources = super::super::client::ServerCapabilities {
        resources: true,
        ..Default::default()
    };
    assert!(supports_action(&resources, "resource_read"));
    assert!(!supports_action(&resources, "prompt_get"));
    assert!(!supports_action(&resources, "resource_complete"));
    let complete = super::super::client::ServerCapabilities {
        resources: true,
        completions: true,
        ..Default::default()
    };
    assert!(supports_action(&complete, "resource_complete"));
}

#[test]
fn feature_actions_map_to_exact_mcp_methods() {
    assert_eq!(
        feature_request("resource_list", &json!({})).unwrap(),
        ("resources/list", json!({}))
    );
    assert_eq!(
        feature_request("resource_templates", &json!({ "cursor": "next" })).unwrap(),
        ("resources/templates/list", json!({ "cursor": "next" }))
    );
    assert_eq!(
        feature_request("resource_read", &json!({ "uri": "file:///a" })).unwrap(),
        ("resources/read", json!({ "uri": "file:///a" }))
    );
    assert_eq!(
        feature_request(
            "prompt_get",
            &json!({ "name": "review", "arguments": { "pr": "7" } })
        )
        .unwrap(),
        (
            "prompts/get",
            json!({ "name": "review", "arguments": { "pr": "7" } })
        )
    );
    assert_eq!(
        feature_request(
            "prompt_complete",
            &json!({ "name": "review", "argument": { "name": "pr", "value": "7" } })
        )
        .unwrap(),
        (
            "completion/complete",
            json!({ "ref": { "type": "ref/prompt", "name": "review" }, "argument": { "name": "pr", "value": "7" } })
        )
    );
}

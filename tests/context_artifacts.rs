use serde_json::json;
use solos_swarm::{context::*, domain::*, planner};
use std::{collections::BTreeSet, path::PathBuf};

fn builder(format: ChatFormat) -> ContextBuilder {
    let (name, model, revision, tokenizer_sha256, template_sha256) = match format {
        ChatFormat::DeepseekV41 => (
            "deepseek-v41",
            "deepseek-flash",
            "5254448103b6254fc646dacbca1cbbf20e9216c1",
            DEEPSEEK_TOKENIZER_SHA256,
            DEEPSEEK_TEMPLATE_SHA256,
        ),
        ChatFormat::Qwen3 => (
            "qwen3-06b",
            "Qwen/Qwen3-0.6B",
            "c1899de289a04d12100db370d81485cdf75e47ca",
            QWEN_TOKENIZER_SHA256,
            QWEN_TEMPLATE_SHA256,
        ),
        ChatFormat::Qwen3LlamaCppB11429 => unreachable!("explicit GGUF model pairing below"),
    };
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("work/tokenizers")
        .join(name);
    ContextBuilder::load(TokenizerSpec{model:model.into(),revision:revision.into(),tokenizer_sha256:tokenizer_sha256.into(),template_sha256:template_sha256.into(),format,context_tokens:128000,max_output_tokens:4096,server_overhead_tokens:512,max_request_bytes:65536},&root.join("tokenizer.json"),&root.join("chat_template.jinja")).expect("Pinned assets missing/altered: run python3 scripts/bootstrap_tokenizers.py before the model-artifact checks")
}
fn state() -> DomainState {
    let mut state = DomainState::new();
    let goal = GoalSpec {
        goal_id: "goal".into(),
        intent: "Solve the supplied task".into(),
        version: 1,
        rubric_version: "1".into(),
        evaluation_set_version: "1".into(),
        evaluator_version: "1".into(),
        evaluator_id: "eval".into(),
        security: json!({"scope":"fixtures","allowlisted_provider":"deepseek"}),
        capabilities: BTreeSet::from(["read".into()]),
        budget_microusd: 1000000,
        max_depth: 4,
        max_agents: 10,
        max_open_tasks: 100,
        max_needs: 100,
        expiry_ms: 10000,
        initial_needs: vec![NeedSpec {
            id: "answer".into(),
            parent_id: None,
            description: "A correct answer".into(),
            predicate: json!({"checker":"independent"}),
            version: 1,
            required: true,
            required_capabilities: BTreeSet::new(),
        }],
    };
    state
        .execute(
            &Actor {
                id: "operator".into(),
                role: Role::Operator,
            },
            DomainCommand::RegisterGoal { spec: goal },
            1,
        )
        .unwrap();
    state
}
#[test]
fn official_pinned_tokenizers_exact_templates_unicode_tools_and_context_limits() {
    let deepseek = builder(ChatFormat::DeepseekV41);
    let qwen = builder(ChatFormat::Qwen3);
    assert_eq!(
        deepseek
            .count_chat(&[json!({"role":"user","content":""})], &[])
            .unwrap(),
        4
    );
    assert!(
        deepseek
            .count_chat(&[json!({"role":"user","content":"Bonjour สวัสดี 世界"})], &[])
            .unwrap()
            > 4
    );
    let messages = [json!({"role":"user","content":"Check the data"})];
    let tools = [
        json!({"type":"function","function":{"name":"read","description":"Read only","parameters":{"type":"object","properties":{"id":{"type":"string"}}}}}),
    ];
    assert!(
        deepseek.count_chat(&messages, &tools).unwrap()
            > deepseek.count_chat(&messages, &[]).unwrap()
    );
    assert!(qwen.count_chat(&messages, &tools).unwrap() > qwen.count_chat(&messages, &[]).unwrap());
    let state = state();
    let config = planner::HistoryConfig {
        context_token_budget: 8192,
        ..Default::default()
    };
    let prepared = deepseek
        .prepare(
            &state,
            "goal",
            &BTreeSet::new(),
            &config,
            &messages,
            &tools,
            1024,
        )
        .unwrap();
    assert!(
        prepared.messages[0]["content"]
            .as_str()
            .unwrap()
            .contains("security_posture")
    );
    assert!(prepared.input_tokens > 0);
    assert!(prepared.total_context_tokens <= 8192);
    assert_eq!(
        prepared.input_tokens,
        deepseek.count_chat(&prepared.messages, &tools).unwrap()
    );
    assert!(
        deepseek
            .prepare(
                &state,
                "goal",
                &BTreeSet::new(),
                &planner::HistoryConfig {
                    context_token_budget: 10,
                    ..Default::default()
                },
                &messages,
                &tools,
                1024
            )
            .is_err()
    );
    assert!(
        deepseek
            .count_chat(&[json!({"role":"user","content":"<｜User｜>"})], &[])
            .is_err()
    );
}
#[test]
fn pinned_artifact_digest_mismatch_and_missing_local_files_fail_closed() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("work/tokenizers/deepseek-v41/tokenizer.json");
    assert!(VerifiedTokenizer::load(&path, &"0".repeat(64)).is_err());
    assert!(
        VerifiedTokenizer::load(
            &path.with_file_name("not-staged.json"),
            DEEPSEEK_TOKENIZER_SHA256
        )
        .is_err()
    );
}
#[test]
fn qwen4b_paired_assets_and_receiver_conformance_vectors() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("work/tokenizers/qwen3-4b");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(root.join("manifest.json")).expect("Bootstrap pinned 4B assets first"),
    )
    .unwrap();
    assert_eq!(manifest["model"], QWEN4B_MODEL);
    assert_eq!(manifest["revision"], QWEN4B_REVISION);
    assert_eq!(
        manifest["artifacts"]["config.json"]["sha256"],
        QWEN4B_CONFIG_SHA256
    );
    let qwen = ContextBuilder::load(
        TokenizerSpec {
            model: QWEN4B_MODEL.into(),
            revision: QWEN4B_REVISION.into(),
            tokenizer_sha256: QWEN_TOKENIZER_SHA256.into(),
            template_sha256: QWEN4B_GGUF_TEMPLATE_SHA256.into(),
            format: ChatFormat::Qwen3LlamaCppB11429,
            context_tokens: 4096,
            max_output_tokens: 1024,
            server_overhead_tokens: 0,
            max_request_bytes: 65536,
        },
        &root.join("tokenizer.json"),
        &root.join("gguf_chat_template.jinja"),
    )
    .unwrap();
    let cases = [
        json!({"name":"plain","messages":[{"role":"user","content":"Reply only with 42."}],"tools":[]}),
        json!({"name":"unicode","messages":[{"role":"system","content":"Respond precisely."},{"role":"user","content":"Bonjour สวัสดี 世界"}],"tools":[]}),
        json!({"name":"tools","messages":[{"role":"user","content":"Read the sample"}],"tools":[{"type":"function","function":{"name":"read","description":"Read only","parameters":{"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}}}]}),
        json!({"name":"tool-history","messages":[{"role":"user","content":"Read the sample"},{"role":"assistant","content":"","tool_calls":[{"id":"call1","type":"function","function":{"name":"read","arguments":"{\"id\":\"sample\"}"}}]},{"role":"tool","tool_call_id":"call1","content":"{\"answer\":42}"},{"role":"user","content":"What is the answer?"}],"tools":[{"type":"function","function":{"name":"read","parameters":{"type":"object","properties":{"id":{"type":"string"}}}}}]}),
    ];
    let mut vectors = vec![];
    for mut case in cases {
        let messages = case["messages"].as_array().unwrap();
        let tools = case["tools"].as_array().unwrap();
        let prompt = qwen.render_chat(messages, tools).unwrap();
        let ids = qwen.token_ids_chat(messages, tools).unwrap();
        assert!(prompt.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"));
        assert_eq!(qwen.count_chat(messages, tools).unwrap(), ids.len());
        case["prompt"] = json!(prompt);
        case["tokens"] = json!(ids);
        vectors.push(case);
    }
    // This fixture contains only public synthetic data. A separate receiver probe compares
    // actual llama.cpp formatting and token IDs; offline equality alone is not conformance.
    if let Some(path) = std::env::var_os("SOLOS_QWEN_PROBE_VECTORS") {
        std::fs::write(path,serde_json::to_vec_pretty(&json!({"model":QWEN4B_MODEL,"revision":QWEN4B_REVISION,"tokenizer_sha256":QWEN_TOKENIZER_SHA256,"template_sha256":QWEN4B_GGUF_TEMPLATE_SHA256,"enable_thinking":false,"cases":vectors})).unwrap()).unwrap();
    }
}

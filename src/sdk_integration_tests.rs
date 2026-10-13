//! Explicit real-KVM qualification probe. The ordinary unit suite does not require a hypervisor.
use crate::{
    artifacts::ArtifactStore,
    ledger::{PriceTable, Rate},
    node::{ToolDefinition, receiver_tests::Harness},
    sandbox::{MicroVmAssets, PinnedAsset, ProcessLimits, ToolLimits},
    security::{CapabilityKind, OperationClass, test_support},
    tools::{InstalledConfig, InstalledTools, NativeBackend, TargetEndpoint},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io::Read, path::PathBuf, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires explicitly supplied verified Firecracker/KVM assets; qualification runs it with --ignored"]
async fn actual_microvm_sdk_uses_normal_broker_and_preserves_private_lineage() {
    let asset_path =
        std::env::var_os("SOLOS_SANDBOX_ASSETS").expect("SOLOS_SANDBOX_ASSETS required");
    let manifest: Value = serde_json::from_slice(&std::fs::read(asset_path).unwrap()).unwrap();
    let assets = &manifest["assets"];
    let pin = |name: &str| PinnedAsset {
        path: PathBuf::from(assets[name]["path"].as_str().unwrap()),
        sha256: assets[name]["sha256"].as_str().unwrap().into(),
    };
    let vm = MicroVmAssets {
        firecracker: pin("firecracker-v1.17.0-x86_64"),
        jailer: pin("jailer-v1.17.0-x86_64"),
        kernel: pin("kernel"),
        rootfs: pin("rootfs-v6.ext4"),
        uid: 1000,
        gid: 1000,
        guest_cid: 93,
    };
    for asset in [&vm.firecracker, &vm.jailer, &vm.kernel, &vm.rootfs] {
        asset.verify().unwrap();
    }
    let proof_root = PathBuf::from(
        std::env::var_os("SOLOS_SDK_PROOF")
            .expect("SOLOS_SDK_PROOF owned fresh directory required"),
    );
    assert!(proof_root.is_absolute() && !proof_root.exists());
    std::fs::create_dir_all(&proof_root).unwrap();
    ArtifactStore::create_key(&proof_root.join("artifact.key")).unwrap();
    struct RemoveKey(PathBuf);
    impl Drop for RemoveKey {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _remove_key = RemoveKey(proof_root.join("artifact.key"));
    let mut test_binary = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
    let mut digest = Sha256::new();
    let mut chunk = vec![0; 1024 * 1024];
    loop {
        let count = test_binary.read(&mut chunk).unwrap();
        if count == 0 {
            break;
        }
        digest.update(&chunk[..count]);
    }
    let test_binary_sha256 = format!("{:x}", digest.finalize());
    let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_address = target_listener.local_addr().unwrap();
    let target_server = tokio::spawn(async move {
        let (mut connection, _) = target_listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let count = connection.read(&mut request).await.unwrap();
        assert!(
            std::str::from_utf8(&request[..count])
                .unwrap()
                .starts_with("GET /data HTTP/1.1\r\n")
        );
        connection.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 18\r\nConnection: close\r\n\r\nSCOPED_PRIVATE_GET").await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(300),
                target_listener.accept()
            )
            .await
            .is_err(),
            "stable SDK request must not execute target twice"
        );
    });
    let definition = |name: &str, resource: &str| ToolDefinition {
        name: name.into(),
        description: "owned qualification fixture".into(),
        parameters: if name == "native_execute" {
            json!({"type":"object","properties":{"code":{"type":"string"},"input":{"type":"string"},"broker_tools":{"type":"array","items":{"type":"string"}}},"required":["code","input"],"additionalProperties":false})
        } else {
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false})
        },
        operation: name.into(),
        kind: CapabilityKind::ToolCall,
        resource: resource.into(),
        class: OperationClass::ReadOnly,
    };
    let native = definition("native_execute", "tool/g/native_execute");
    let target = definition("target_read", "owned-target");
    let fixture = test_support::fixture();
    let config = InstalledConfig {
        owned_directory: proof_root.join("invocations"),
        artifacts: proof_root.join("artifacts"),
        artifact_key: proof_root.join("artifact.key"),
        wasi_limits: ToolLimits {
            fuel: 100_000,
            memory_bytes: 65536,
            deadline_ms: 5000,
            output_bytes: 4096,
        },
        native_limits: ProcessLimits {
            cpu_millicores: 1000,
            memory_bytes: 512 * 1024 * 1024,
            pids: 128,
            max_output_bytes: 65536,
        },
        native_deadline_ms: 30000,
        native: Some(NativeBackend::V { assets: vm }),
        price: fixture
            .signer
            .sign(
                "price-table",
                PriceTable {
                    version: "sdk-proof-v1".into(),
                    provider: "local-tools".into(),
                    model: "sandbox".into(),
                    currency: "microusd".into(),
                    not_before: fixture.now,
                    expires: fixture.now + 600,
                    rates: ["elapsed_ms", "output_bytes", "fuel"]
                        .into_iter()
                        .map(|name| {
                            (
                                name.into(),
                                Rate {
                                    numerator: 1,
                                    denominator: 1,
                                },
                            )
                        })
                        .collect(),
                },
            )
            .unwrap(),
        export_roots: BTreeMap::new(),
        broker_calls: vec![target.clone()],
        broker_budget_microusd: 1000,
        targets: [(
            "owned-target".into(),
            TargetEndpoint {
                endpoint: format!("http://{target_address}"),
                paths: ["/data".into()].into(),
                ca_pem: None,
                timeout_ms: 5000,
                max_response_bytes: 1024,
                markings: ["sdk-private".into()].into(),
            },
        )]
        .into(),
    };
    std::fs::write(
        proof_root.join("tools.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let h = Harness::open_with_tools(
        Arc::new(InstalledTools::open(&proof_root.join("tools.json")).unwrap()),
        vec![native, target],
    )
    .await;
    assert_eq!(
        h.send("sdk-claim", "/tool/claim_task", json!({})).await.0,
        200
    );
    let code = "import os,json,base64,swarm_broker\nassert not os.path.exists('/identity/identity.json')\nassert 'SWARM_IDENTITY_DIR' not in os.environ\ndenied=0\nfor path in ['/model','/tool/native_execute']:\n try: swarm_broker.request(path,{},request_id='deny-'+str(denied))\n except RuntimeError: denied+=1\nassert denied==2\na=swarm_broker.request('/tool/target_read',{'path':'/data'},request_id='sdk-target-read')\nb=swarm_broker.request('/tool/target_read',{'path':'/data'},request_id='sdk-target-read')\nassert a==b\nassert base64.b64decode(a['body'])==b'SCOPED_PRIVATE_GET'\nprint(json.dumps({'marker':'NORMAL_BROKER_SDK_PASS','denied':denied,'target_status':a['status']}))";
    let (status, value) = h
        .send(
            "sdk-native",
            "/tool/native_execute",
            json!({"code":code,"input":"","broker_tools":["target_read"]}),
        )
        .await;
    assert_eq!(status, 200, "native broker result {value}");
    let output: Value =
        serde_json::from_slice(&STANDARD.decode(value["stdout"].as_str().unwrap()).unwrap())
            .unwrap();
    assert_eq!(output["marker"], "NORMAL_BROKER_SDK_PASS");
    target_server.await.unwrap();
    assert!(
        h.upstream.lock().await.is_empty(),
        "tool child cannot call provider"
    );
    let state = h.kernel.state().await.unwrap();
    let source = &state.security.inputs["tool-result/tool-sdk-native"];
    assert!(source.markings.contains("sdk-private"));
    assert!(source.parents.contains("tool-result/tool-sdk-target-read"));
    assert!(
        state.security.observed[h.fixture.subject.id()]
            .contains("tool-result/tool-sdk-target-read")
    );
    assert_eq!(state.effects["tool-sdk-native"].state, "done");
    assert_eq!(h.send("sdk-private-provider-denied","/model",json!({"model":"m1","messages":[{"role":"user","content":"describe my tool result"}],"max_tokens":16})).await.0,403);
    assert!(
        h.upstream.lock().await.is_empty(),
        "private tool lineage cannot leave through provider receiver"
    );
    assert_eq!(state.ledger.accounts["global"].total.held, 0);
    let view = h.kernel.cluster.view().await.unwrap();
    assert!(
        view.kv
            .iter()
            .filter(|(key, _)| key.starts_with("tools/bindings/"))
            .all(|(_, value)| value["status"] == "retired")
    );
    assert!(
        !view
            .kv
            .keys()
            .any(|key| key.starts_with("allocations/tool-"))
    );
    let native_directory = proof_root
        .join("invocations")
        .join(crate::kernel::hex(&crate::audit::hash(b"tool-sdk-native")));
    let cleanup: Value =
        serde_json::from_slice(&std::fs::read(native_directory.join("cleanup.json")).unwrap())
            .unwrap();
    assert_eq!(cleanup["verdict"], "PASS");
    let proof = json!({"verdict":"PASS","scope":"actual Tier V SDK, normal authenticated Broker and owned fixed GET fixture on remote-ai; independent-host qualification separate",
        "guest":output,"target_calls":1,"provider_calls":0,"host_signer_only":true,"private_lineage_transferred":true,
        "private_provider_egress_denied":true,"test_binary_sha256":test_binary_sha256,
        "source_revision":std::env::var("SOLOS_SOURCE_REVISION").unwrap_or_else(|_|"uncommitted-source-probe".into()),
        "held_microusd":0,"tool_allocation_retired":true,"cleanup":cleanup,
        "asset_manifest_sha256":crate::kernel::hex(&crate::audit::hash(&serde_json::to_vec(&manifest).unwrap()))});
    std::fs::write(
        proof_root.join("proof.json"),
        serde_json::to_vec_pretty(&proof).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(proof_root.join("artifact.key")).unwrap();
    println!("{}", serde_json::to_string(&proof).unwrap());
    h.close().await;
}

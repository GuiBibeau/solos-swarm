//! The Goal's fetch policy and the code that enforces it. A loopback server
//! stands in for the target, so nothing here leaves the machine.
use super::*;
use axum::{
    body::Body,
    http::Uri,
    response::{IntoResponse, Response},
};
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};

fn goal_json(scope: &str, fetch: Option<Value>) -> Value {
    let mut goal = json!({"schema_version":1,"intent":"test","worker_prompt":"test","synthesis_prompt":"test","scope":scope,"model":"deepseek-flash","agent_count":2,"deadline_seconds":600,"budget_microusd":1_000_000,"max_calls_per_agent":50,"max_input_bytes":65536,"max_output_tokens":2048,"input_price_microusd_per_million":300000,"output_price_microusd_per_million":1200000,"grants":["claim_task","board_read","board_post","submit_finding","read_findings","submit_artifact","fetch_url"],"tasks":[{"id":"a","input":{}},{"id":"b","input":{}}],"seed":1,"catalog_version":"test","scorer_version":"1"});
    if let Some(fetch) = fetch {
        goal["fetch"] = fetch;
    }
    goal
}
fn goal(fetch: Option<Value>) -> Goal {
    serde_json::from_value(goal_json("synthetic-fixtures-only", fetch)).unwrap()
}
fn zelix_policy() -> Value {
    json!({"hosts":["zelix-front-dev.fly.dev","zelix-deck.vercel.app"],"methods":["GET","HEAD","OPTIONS"],"per_agent_rps":1,"follow_redirects_off_list":false})
}
fn policy(value: Value) -> FetchPolicy {
    serde_json::from_value(value).unwrap()
}
fn url(raw: &str) -> reqwest::Url {
    reqwest::Url::parse(raw).unwrap()
}
fn expect_refused(result: Result<(), FetchRefused>) -> FetchRefused {
    result.expect_err("the request should be refused")
}

#[test]
fn a_policy_is_checked_when_the_goal_is() {
    let goal = goal(Some(zelix_policy()));
    goal.validate().unwrap();
    // The policy the issue names round-trips and keeps its meaning.
    let fetch = goal.fetch.as_ref().unwrap();
    assert_eq!(fetch.per_agent_rps, Some(1.0));
    assert!(!fetch.follow_redirects_off_list && !fetch.allow_private_networks);

    let invalid = |fetch: Value, why: &str| {
        let goal =
            serde_json::from_value::<Goal>(goal_json("synthetic-fixtures-only", Some(fetch)))
                .unwrap();
        let error = format!("{:#}", goal.validate().unwrap_err());
        assert!(error.contains(why), "{error} should mention {why}");
    };
    invalid(json!({"hosts":[]}), "fetch.hosts must list");
    invalid(json!({"hosts":["https://zelix.dev"]}), "host name only");
    invalid(json!({"hosts":["zelix.dev:8443"]}), "host name only");
    invalid(json!({"hosts":["zelix.dev/api"]}), "host name only");
    invalid(json!({"hosts":["zelix.dev "]}), "host name only");
    invalid(json!({"hosts":["zelix..dev"]}), "host name only");
    invalid(json!({"hosts":["-zelix.dev"]}), "host name only");
    invalid(json!({"hosts":["*"]}), "host name only");
    invalid(json!({"hosts":["*.com"]}), "two or more labels");
    invalid(json!({"hosts":["a.*.zelix.dev"]}), "host name only");
    invalid(json!({"hosts":["z\u{e9}lix.dev"]}), "punycode");
    invalid(json!({"hosts":["zelix.dev"],"methods":[]}), "fetch.methods");
    invalid(json!({"hosts":["zelix.dev"],"methods":["TRACE"]}), "TRACE");
    for rps in [0.0, -1.0, 0.001, 5000.0] {
        invalid(
            json!({"hosts":["zelix.dev"],"per_agent_rps":rps}),
            "per_agent_rps",
        );
    }
    // Unknown keys and a missing host list are refused at parse time, so a
    // typo cannot leave a Goal looking restricted when it is not.
    assert!(
        serde_json::from_value::<Goal>(goal_json(
            "synthetic-fixtures-only",
            Some(json!({"hosts":["zelix.dev"],"method":["GET"]}))
        ))
        .is_err()
    );
    assert!(
        serde_json::from_value::<Goal>(goal_json(
            "synthetic-fixtures-only",
            Some(json!({"methods":["GET"]}))
        ))
        .is_err()
    );
    // A policy for a tool the Goal does not grant limits nothing.
    let mut ungranted = goal.clone();
    ungranted.grants.retain(|grant| grant != "fetch_url");
    assert!(
        format!("{:#}", ungranted.validate().unwrap_err()).contains("needs the fetch_url grant")
    );
    // Both spellings of an address, a wildcard and the policy's whole range pass.
    for fetch in [
        json!({"hosts":["10.1.2.3","[::1]","*.fly.dev","App.Example.COM."],"methods":["get","Head"],"per_agent_rps":0.01}),
        json!({"hosts":["zelix.dev"],"per_agent_rps":1000,"follow_redirects_off_list":true,"allow_private_networks":true}),
    ] {
        serde_json::from_value::<Goal>(goal_json("synthetic-fixtures-only", Some(fetch)))
            .unwrap()
            .validate()
            .unwrap();
    }
}

#[test]
fn a_goal_template_sets_the_policy_in_yaml() {
    // Agent mode renders a swarm Ticket's Goal from YAML, where the rate is
    // an integer and a wildcard is a quoted scalar.
    let mut text = serde_yaml::to_string(&goal_json("synthetic-fixtures-only", None)).unwrap();
    text.push_str(
        "fetch:\n  hosts: [app.example.com, \"*.example.org\"]\n  methods: [GET, HEAD]\n  per_agent_rps: 1\n",
    );
    let goal: Goal = serde_yaml::from_str(&text).unwrap();
    goal.validate().unwrap();
    let fetch = goal.fetch.unwrap();
    assert_eq!(fetch.hosts, ["app.example.com", "*.example.org"]);
    assert_eq!(fetch.per_agent_rps, Some(1.0));
}

#[test]
fn the_zelix_audit_scope_is_admitted_with_a_policy_and_the_scope_list_stays_closed() {
    let zelix = "authorized-audit:zelix";
    let with = |scope: &str, fetch: Option<Value>, fetches: bool| {
        let mut goal: Goal = serde_json::from_value(goal_json(scope, fetch)).unwrap();
        if !fetches {
            goal.grants.retain(|grant| grant != "fetch_url");
        }
        goal.validate()
    };
    with(zelix, Some(zelix_policy()), true).unwrap();
    // The scope that grants live probing must say, in a policy, where.
    let error = format!("{:#}", with(zelix, None, true).unwrap_err());
    assert!(error.contains("must carry a fetch policy"), "{error}");
    // Without fetch_url it probes nothing, so a source-review Goal needs none.
    with(zelix, None, false).unwrap();
    // The scopes frozen before the policy keep validating as they did.
    for legacy in AUDIT_SCOPES_BEFORE_FETCH_POLICY {
        with(legacy, None, true).unwrap();
        with(legacy, Some(zelix_policy()), true).unwrap();
    }
    for refused in [
        "authorized-audit:other-target",
        "authorized-audit:",
        "authorized-audit",
    ] {
        assert!(
            with(refused, Some(zelix_policy()), true).is_err(),
            "{refused}"
        );
    }
}

#[test]
fn a_goal_without_a_policy_is_unchanged() {
    let goal = goal(None);
    goal.validate().unwrap();
    // The frozen Goal's bytes and digest do not gain a key.
    assert!(serde_json::to_value(&goal).unwrap().get("fetch").is_none());
    let guard = FetchGuard::new(goal.fetch.as_ref());
    for (raw, method) in [
        ("https://anywhere.example/x", "GET"),
        ("http://other.example:8080/", "POST"),
        ("https://another.example/", "DELETE"),
    ] {
        guard.precheck(&json!({"url":raw,"method":method})).unwrap();
        guard.admit(&url(raw), method, None).unwrap();
    }
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&dir).unwrap();
    let mut store = Store::new(&dir, &goal).unwrap();
    for _ in 0..100 {
        store
            .fetch_pace(&goal, "agent-01", "https://x.example/")
            .unwrap();
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn hosts_match_exactly_or_by_suffix_and_nothing_else() {
    let policy =
        policy(json!({"hosts":["Zelix-Deck.vercel.app.","*.fly.dev","192.0.2.7","[2001:db8::7]"]}));
    let guard = FetchGuard::new(Some(&policy));
    let admitted = |raw: &str| guard.admit(&url(raw), "GET", None).is_ok();
    // Exact, whatever the case, trailing dot or port.
    assert!(admitted("https://zelix-deck.vercel.app/a"));
    assert!(admitted("https://ZELIX-DECK.vercel.app:8443/a?b=c"));
    assert!(admitted("https://zelix-deck.vercel.app./a"));
    // `*.suffix` is every name under the suffix, not the suffix itself.
    assert!(admitted("https://zelix-front-dev.fly.dev/"));
    assert!(admitted("https://a.b.fly.dev/"));
    assert!(!admitted("https://fly.dev/"));
    // Lookalikes, parents, siblings and userinfo tricks are not the host.
    assert!(!admitted("https://evil-fly.dev/"));
    assert!(!admitted("https://fly.dev.evil.example/"));
    assert!(!admitted("https://vercel.app/"));
    assert!(!admitted("https://other.vercel.app/"));
    assert!(!admitted("https://zelix-deck.vercel.app.evil.example/"));
    assert!(!admitted("https://zelix-deck.vercel.app@evil.example/"));
    assert!(!admitted("https://evil.example/?h=zelix-deck.vercel.app"));
    // Addresses, in either family, match as addresses.
    assert!(admitted("http://192.0.2.7/"));
    assert!(!admitted("http://192.0.2.8/"));
    assert!(admitted("http://[2001:db8::7]:8080/"));
    // Decimal and hex spellings are one address to the URL parser.
    assert!(admitted("http://3221225991/"));
    assert!(!admitted("http://2130706433/"));
}

#[test]
fn a_method_off_the_list_is_refused_before_anything_is_sent() {
    let policy = policy(zelix_policy());
    let guard = FetchGuard::new(Some(&policy));
    let ok = url("https://zelix-deck.vercel.app/");
    for method in ["GET", "HEAD", "OPTIONS"] {
        guard.admit(&ok, method, None).unwrap();
        guard
            .precheck(&json!({"url":ok.as_str(),"method":method.to_lowercase()}))
            .unwrap();
    }
    guard.precheck(&json!({"url":ok.as_str()})).unwrap();
    for method in ["POST", "PUT", "PATCH", "DELETE"] {
        let refused = expect_refused(guard.precheck(&json!({"url":ok.as_str(),"method":method})));
        assert_eq!(refused.reason, "method_not_allowed");
        assert!(
            refused
                .message
                .contains(&format!("method {method} is not allowed"))
                && refused.message.contains("GET, HEAD, OPTIONS"),
            "{refused}"
        );
        assert_eq!(refused.hop, 0);
        assert!(refused.retry_after_ms.is_none());
    }
    // A host and a method both wrong is reported as the host.
    let refused =
        expect_refused(guard.precheck(&json!({"url":"https://evil.example/","method":"POST"})));
    assert_eq!(refused.reason, "host_not_allowed");
    assert!(
        refused
            .message
            .contains("zelix-front-dev.fly.dev, zelix-deck.vercel.app"),
        "the model is told where it may go: {refused}"
    );
}

#[test]
fn a_host_header_of_the_agents_own_would_go_around_the_host_rule() {
    let policy = policy(zelix_policy());
    let guard = FetchGuard::new(Some(&policy));
    let args = |headers: Value| json!({"url":"https://zelix-deck.vercel.app/","headers":headers});
    guard
        .precheck(&args(
            json!({"Accept":"text/html","Origin":"https://evil.example"}),
        ))
        .unwrap();
    for name in ["Host", "host", "HOST"] {
        let refused =
            expect_refused(guard.precheck(&args(json!({name:"other-tenant.vercel.app"}))));
        assert_eq!(refused.reason, "header_not_allowed");
        assert!(refused.message.contains("Nothing was sent"), "{refused}");
    }
    // A Goal with no policy never judged headers.
    FetchGuard::new(None)
        .precheck(&args(json!({"Host":"other-tenant.vercel.app"})))
        .unwrap();
}

#[test]
fn a_redirect_is_judged_by_its_own_host() {
    let strict = policy(zelix_policy());
    let from = url("https://zelix-deck.vercel.app/old");
    let via = |hop| fetch_policy::Redirect {
        hop,
        status: 302,
        from: &from,
    };
    let guard = FetchGuard::new(Some(&strict));
    // Staying on the list is followed, and a redirect repeats no method check.
    guard
        .admit(
            &url("https://zelix-front-dev.fly.dev/new"),
            "GET",
            Some(&via(1)),
        )
        .unwrap();
    let off = url("https://accounts.google.com/o/oauth2/auth");
    let refused = expect_refused(guard.admit(&off, "GET", Some(&via(3))));
    assert_eq!(refused.reason, "redirect_off_list");
    assert_eq!(refused.hop, 3);
    assert_eq!(refused.url, off.as_str());
    assert!(
        refused
            .message
            .contains("https://zelix-deck.vercel.app/old answered 302")
            && refused.message.contains("accounts.google.com")
            && refused.message.contains("not followed"),
        "{refused}"
    );

    let loose = policy(json!({"hosts":["zelix-deck.vercel.app"],"follow_redirects_off_list":true}));
    let guard = FetchGuard::new(Some(&loose));
    guard.admit(&off, "GET", Some(&via(1))).unwrap();
    // The URL the agent asked for is still held to the list.
    assert_eq!(
        expect_refused(guard.admit(&off, "GET", None)).reason,
        "host_not_allowed"
    );
}

#[test]
fn each_agent_has_its_own_pace_and_a_refusal_costs_nothing() {
    let mut pacer = Pacer::new();
    assert_eq!(interval_ms(1.0), 1000);
    assert_eq!(interval_ms(0.5), 2000);
    assert_eq!(interval_ms(3.0), 334, "rounded up, never faster than asked");
    assert_eq!(interval_ms(1000.0), 1);
    assert_eq!(pacer.take("agent-01", 1000, 5_000), Ok(()));
    assert_eq!(pacer.take("agent-01", 1000, 5_400), Err(600));
    assert_eq!(pacer.take("agent-01", 1000, 5_999), Err(1));
    // Another agent is not slowed by the first, and the first's refused
    // calls did not push its window out.
    assert_eq!(pacer.take("agent-02", 1000, 5_400), Ok(()));
    assert_eq!(pacer.take("agent-01", 1000, 6_000), Ok(()));
    assert_eq!(pacer.take("agent-01", 1000, 6_000), Err(1000));
}

#[test]
fn the_store_paces_fetch_url_per_agent_with_a_wait_the_model_can_use() {
    let goal = goal(Some(json!({"hosts":["zelix.dev"],"per_agent_rps":0.5})));
    let dir = std::env::temp_dir().join(uuid::Uuid::new_v4().to_string());
    fs::create_dir_all(&dir).unwrap();
    let mut store = Store::new(&dir, &goal).unwrap();
    store
        .fetch_pace(&goal, "agent-01", "https://zelix.dev/")
        .unwrap();
    let refused = store
        .fetch_pace(&goal, "agent-01", "https://zelix.dev/again")
        .unwrap_err();
    assert_eq!(refused.reason, "rate_limited");
    let wait = refused.retry_after_ms.unwrap();
    assert!((1..=2000).contains(&wait), "{wait}");
    assert!(
        refused.message.contains("0.5 fetch_url call(s) per second")
            && refused.message.contains(&format!("Retry after {wait} ms")),
        "{refused}"
    );
    assert_eq!(refused.url, "https://zelix.dev/again");
    assert_eq!(refused.reply()["retry_after_ms"], wait);
    assert_eq!(refused.reply()["refused"], true);
    store
        .fetch_pace(&goal, "agent-02", "https://zelix.dev/")
        .unwrap();
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_public_addresses_are_public() {
    let public = [
        "8.8.8.8",
        "1.1.1.1",
        "93.184.216.34",
        "172.15.255.255",
        "172.32.0.0",
        "100.63.255.255",
        "100.128.0.0",
        "2606:4700:4700::1111",
        "2a00:1450:4001:81b::200e",
        "::ffff:8.8.8.8",
        "64:ff9b::808:808",
    ];
    let private = [
        "127.0.0.1",
        "127.8.9.10",
        "0.0.0.0",
        "0.1.2.3",
        "10.0.0.1",
        "172.16.0.1",
        "172.31.255.255",
        "192.168.1.1",
        "169.254.169.254",
        "100.64.0.1",
        "100.127.255.255",
        "192.0.0.8",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "198.18.0.1",
        "224.0.0.1",
        "240.0.0.1",
        "255.255.255.255",
        "::1",
        "::",
        "fe80::1",
        "fc00::1",
        "fd12:3456::1",
        "fec0::1",
        "ff02::1",
        "2001:db8::1",
        "100::1",
        "::ffff:127.0.0.1",
        "::ffff:10.0.0.1",
        "::ffff:169.254.169.254",
        "::127.0.0.1",
        "64:ff9b::7f00:1",
        "2002:7f00:1::1",
        "2002:a9fe:a9fe::1",
    ];
    for ip in public {
        assert!(is_public_ip(ip.parse().unwrap()), "{ip} is public");
    }
    for ip in private {
        assert!(!is_public_ip(ip.parse().unwrap()), "{ip} is not public");
    }
}

/// A target on a loopback port: `/ok` answers, the others redirect.
struct Site {
    hits: AtomicUsize,
    port: u16,
}

async fn serve(State(site): State<Arc<Site>>, uri: Uri) -> Response {
    site.hits.fetch_add(1, Ordering::SeqCst);
    let redirect = |to: String| {
        Response::builder()
            .status(302)
            .header("location", to)
            .body(Body::empty())
            .unwrap()
    };
    match uri.path() {
        "/to-own" => redirect("/ok".into()),
        "/to-off" => redirect("http://elsewhere.test/secret".into()),
        "/to-local" => redirect(format!("http://localhost:{}/ok", site.port)),
        "/to-loopback" => redirect(format!("http://127.0.0.1:{}/secret", site.port)),
        "/to-metadata" => redirect("http://169.254.169.254/latest/meta-data/".into()),
        "/to-rebound" => redirect(format!("http://rebound.example.test:{}/secret", site.port)),
        _ => (StatusCode::OK, "hello").into_response(),
    }
}

async fn site() -> (Arc<Site>, String) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let site = Arc::new(Site {
        hits: AtomicUsize::new(0),
        port,
    });
    let app = Router::new().fallback(serve).with_state(site.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (site, format!("http://127.0.0.1:{port}"))
}

fn refusal_of(error: &anyhow::Error) -> FetchRefused {
    error
        .downcast_ref::<FetchRefused>()
        .unwrap_or_else(|| panic!("a refusal, not a failure: {error:#}"))
        .clone()
}

#[tokio::test]
async fn a_redirect_off_the_list_is_refused_before_it_is_requested() {
    let (site, base) = site().await;
    let policy = policy(json!({"hosts":["127.0.0.1"],"allow_private_networks":true}));
    let guard = FetchGuard::new(Some(&policy));

    // The first hop is on the list and goes out; the hop the server names
    // is not, and never does.
    let error = fetch_guarded(&json!({"url":format!("{base}/to-off")}), &guard)
        .await
        .unwrap_err();
    let refusal = refusal_of(&error);
    assert_eq!(refusal.reason, "redirect_off_list");
    assert_eq!(refusal.hop, 1);
    assert_eq!(refusal.url, "http://elsewhere.test/secret");
    assert!(refusal.message.contains("elsewhere.test"), "{refusal}");
    assert_eq!(site.hits.load(Ordering::SeqCst), 1);

    // A redirect inside the list is followed and reported as before.
    let page = fetch_guarded(&json!({"url":format!("{base}/to-own")}), &guard)
        .await
        .unwrap();
    assert_eq!(page["status"], 200);
    assert_eq!(page["body"], "hello");
    assert_eq!(page["final_url"], format!("{base}/ok"));
    assert_eq!(page["redirects"][0]["status"], 302);
    assert_eq!(site.hits.load(Ordering::SeqCst), 3);

    // A first URL off the list never leaves the broker.
    let error = fetch_guarded(&json!({"url":"http://elsewhere.test/"}), &guard)
        .await
        .unwrap_err();
    assert_eq!(refusal_of(&error).reason, "host_not_allowed");
    assert_eq!(site.hits.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn follow_redirects_off_list_follows_where_the_server_points() {
    let (site, base) = site().await;
    let policy = policy(json!({
        "hosts":["127.0.0.1"],"follow_redirects_off_list":true,"allow_private_networks":true
    }));
    let guard = FetchGuard::new(Some(&policy));
    let page = fetch_guarded(&json!({"url":format!("{base}/to-local")}), &guard)
        .await
        .unwrap();
    assert_eq!(page["status"], 200);
    assert!(
        page["final_url"]
            .as_str()
            .unwrap()
            .starts_with("http://localhost:"),
        "{page}"
    );
    assert_eq!(site.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn private_addresses_are_refused_on_every_hop_unless_the_policy_allows_them() {
    let (site, base) = site().await;
    // Every address below is on the list, so the list alone would let it through.
    let listed = policy(json!({"hosts":[
        "127.0.0.1", "localhost", "::1", "10.0.0.5", "192.168.1.1", "100.64.1.1",
        "169.254.169.254", "::ffff:127.0.0.1"
    ]}));
    let with_policy = FetchGuard::new(Some(&listed));
    let without_policy = FetchGuard::new(None);
    let port = site.port;
    for guard in [&with_policy, &without_policy] {
        for raw in [
            format!("{base}/ok"),
            format!("http://localhost:{port}/ok"),
            format!("http://[::1]:{port}/ok"),
            "http://169.254.169.254/latest/meta-data/".to_owned(),
            "http://10.0.0.5:8080/".to_owned(),
            "http://192.168.1.1/".to_owned(),
            "http://100.64.1.1/".to_owned(),
            "http://[::ffff:127.0.0.1]/".to_owned(),
            "http://2130706433/".to_owned(),
        ] {
            let error = fetch_guarded(&json!({"url":raw}), guard).await.unwrap_err();
            let refusal = refusal_of(&error);
            assert_eq!(refusal.reason, "private_address", "{raw}");
            assert!(
                refusal.message.contains("resolves to a private address")
                    && refusal.message.contains("Nothing was sent"),
                "{refusal}"
            );
        }
    }
    assert_eq!(site.hits.load(Ordering::SeqCst), 0, "no request was sent");

    // A later hop is vetted the same way, by the address it resolves to.
    let hop = with_policy.pin(&url("http://169.254.169.254/"), 2).await;
    let error = hop.unwrap_err();
    assert_eq!(
        (refusal_of(&error).reason, refusal_of(&error).hop),
        ("private_address", 2)
    );

    // The policy's own say-so lets the same fetch through.
    let allowed = policy(json!({"hosts":["127.0.0.1"],"allow_private_networks":true}));
    let page = fetch_guarded(
        &json!({"url":format!("{base}/ok")}),
        &FetchGuard::new(Some(&allowed)),
    )
    .await
    .unwrap();
    assert_eq!(page["status"], 200);
    assert_eq!(site.hits.load(Ordering::SeqCst), 1);
}

struct NoModel;
impl Provider for NoModel {
    fn dispatch(&self, _: Value) -> solos_swarm::gateway::Dispatch {
        Box::pin(async { Err(anyhow::anyhow!("this test has no model")) })
    }
}

/// A broker on a Unix socket for `goal`, with agents 01 and 02 admitted.
async fn serve_broker(goal: Goal) -> (PathBuf, reqwest::Client, [String; 2]) {
    let dir =
        std::env::temp_dir().join(format!("fetch-{}", &uuid::Uuid::new_v4().to_string()[..8]));
    fs::create_dir(&dir).unwrap();
    goal.validate().unwrap();
    let mut store = Store::new(&dir, &goal).unwrap();
    store.goal_status = "running".into();
    let tokens = [
        store.admit("agent-01").unwrap(),
        store.admit("agent-02").unwrap(),
    ];
    let socket = dir.join("b.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let app = router(Broker {
        goal: Arc::new(goal),
        store: Arc::new(Mutex::new(store)),
        provider: Arc::new(NoModel),
        slots: Arc::new(Semaphore::new(1)),
        client: reqwest::Client::new(),
        code: None,
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .unix_socket(socket)
        .build()
        .unwrap();
    (dir, client, tokens)
}

async fn fetch_tool(client: &reqwest::Client, token: &str, args: Value) -> (u16, Value) {
    let response = client
        .post("http://broker/tool/fetch_url")
        .bearer_auth(token)
        .json(&args)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.json().await.unwrap())
}

fn refusals(dir: &Path) -> Vec<Value> {
    fs::read_to_string(dir.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["kind"] == "fetch_refused")
        .map(|event| event["data"].clone())
        .collect()
}

#[tokio::test]
async fn the_broker_refuses_in_a_result_the_model_reads_and_records_each_refusal() {
    let (site, base) = site().await;
    let goal = goal(Some(json!({
        "hosts":["127.0.0.1"],"methods":["GET","HEAD","OPTIONS"],"per_agent_rps":0.2,
        "allow_private_networks":true
    })));
    let (dir, client, [one, two]) = serve_broker(goal).await;

    // Off the list: the model gets the rule as a result, not a bare status.
    let (status, reply) = fetch_tool(&client, &one, json!({"url":"http://elsewhere.test/x"})).await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["refused"], true);
    assert_eq!(reply["reason"], "host_not_allowed");
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("Allowed hosts: 127.0.0.1")
    );
    // A method off the list.
    let (_, reply) = fetch_tool(
        &client,
        &one,
        json!({"url":format!("{base}/ok"),"method":"DELETE"}),
    )
    .await;
    assert_eq!(reply["reason"], "method_not_allowed");
    // A redirect hop off the list is refused after the first hop went out.
    let (_, reply) = fetch_tool(&client, &two, json!({"url":format!("{base}/to-off")})).await;
    assert_eq!(reply["reason"], "redirect_off_list");
    assert_eq!(site.hits.load(Ordering::SeqCst), 1);
    // The refusals above consumed none of agent-02's pace but its call did:
    // within the window the next one waits, and agent-01 is unaffected.
    let (_, reply) = fetch_tool(&client, &two, json!({"url":format!("{base}/ok")})).await;
    assert_eq!(reply["reason"], "rate_limited");
    let wait = reply["retry_after_ms"].as_u64().unwrap();
    assert!((1..=5000).contains(&wait), "{reply}");
    let (status, page) = fetch_tool(&client, &one, json!({"url":format!("{base}/ok")})).await;
    assert_eq!(
        (status, page["status"].clone(), page["body"].clone()),
        (200, json!(200), json!("hello"))
    );
    let (_, reply) = fetch_tool(&client, &one, json!({"url":format!("{base}/ok")})).await;
    assert_eq!(reply["reason"], "rate_limited");
    assert_eq!(
        site.hits.load(Ordering::SeqCst),
        2,
        "refused calls sent nothing"
    );

    let events = refusals(&dir);
    let reasons: Vec<&str> = events
        .iter()
        .map(|e| e["reason"].as_str().unwrap())
        .collect();
    assert_eq!(
        reasons,
        [
            "host_not_allowed",
            "method_not_allowed",
            "redirect_off_list",
            "rate_limited",
            "rate_limited"
        ]
    );
    assert_eq!(events[2]["agent_id"], "agent-02");
    assert_eq!(events[2]["hop"], 1);
    assert_eq!(events[2]["url"], format!("{base}/to-off"));
    assert_eq!(events[2]["refused_url"], "http://elsewhere.test/secret");
    assert_eq!(events[1]["method"], "DELETE");
    assert!(events[3]["retry_after_ms"].as_u64().is_some());
    fs::remove_dir_all(dir).unwrap();
}

#[tokio::test]
async fn without_a_policy_only_private_addresses_are_refused() {
    let (site, base) = site().await;
    let (dir, client, [one, _]) = serve_broker(goal(None)).await;
    // Loopback, as any private address, is refused even with no policy ...
    let (status, reply) = fetch_tool(&client, &one, json!({"url":format!("{base}/ok")})).await;
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["reason"], "private_address");
    let (_, reply) = fetch_tool(
        &client,
        &one,
        json!({"url":"http://169.254.169.254/latest/"}),
    )
    .await;
    assert_eq!(reply["reason"], "private_address");
    assert_eq!(site.hits.load(Ordering::SeqCst), 0);
    assert_eq!(refusals(&dir).len(), 2);
    // ... while a fetch that fails for another reason is still a denied call
    // and a `fetch_failed` event, not a refusal.
    let (status, reply) = fetch_tool(&client, &one, json!({"url":"ftp://files.example/x"})).await;
    assert_eq!(status, 403, "{reply}");
    assert!(
        reply["error"]
            .as_str()
            .unwrap()
            .contains("http and https only"),
        "{reply}"
    );
    assert_eq!(refusals(&dir).len(), 2, "that was a failure, not a refusal");
    fs::remove_dir_all(dir).unwrap();
}

/// What a name that looks public resolves to in these tests:
/// `docs.example.test` is on the public internet, `rebound.example.test`
/// answers with the host's loopback, `mixed.example.test` with both.
fn answers(host: &str) -> Vec<IpAddr> {
    let public: IpAddr = "93.184.216.34".parse().unwrap();
    let loopback: IpAddr = "127.0.0.1".parse().unwrap();
    match host {
        "docs.example.test" => vec![public],
        "rebound.example.test" => vec![loopback],
        "mixed.example.test" => vec![public, loopback],
        other => panic!("{other} was not meant to be looked up"),
    }
}

/// A guard for a fetch with no Goal, whose names resolve as `answers` says
/// and whose connections go to `site`, the only server these tests have.
fn open_guard(allow_private: bool, site: &Site) -> FetchGuard<'static> {
    FetchGuard::open(allow_private)
        .resolving(answers)
        .dialing(SocketAddr::from(([127, 0, 0, 1], site.port)))
}

fn ok(page: &Value) -> (Value, Value) {
    (page["status"].clone(), page["body"].clone())
}

#[tokio::test]
async fn an_open_guard_refuses_every_private_address_and_allows_them_when_told() {
    let (site, base) = site().await;
    let port = site.port;
    let refused = FetchGuard::open(false);
    for raw in [
        format!("{base}/ok"),
        format!("http://localhost:{port}/ok"),
        format!("http://[::1]:{port}/ok"),
        format!("http://[fd00::1]:{port}/ok"),
        format!("http://[fd12:3456:789a::1]:{port}/ok"),
        format!("http://[fe80::1]:{port}/ok"),
        format!("http://[::ffff:127.0.0.1]:{port}/ok"),
        format!("http://[::]:{port}/ok"),
        format!("http://0.0.0.0:{port}/ok"),
        format!("http://2130706433:{port}/ok"),
        "http://10.1.2.3/".to_owned(),
        "http://172.16.0.9/".to_owned(),
        "http://192.168.0.9/".to_owned(),
        "http://169.254.169.254/latest/meta-data/".to_owned(),
        "http://100.64.0.1/".to_owned(),
        "http://100.100.100.100/".to_owned(),
        "http://224.0.0.251/".to_owned(),
        "http://[ff02::fb]/".to_owned(),
    ] {
        let error = fetch_guarded(&json!({"url": raw}), &refused)
            .await
            .unwrap_err();
        let refusal = refusal_of(&error);
        assert_eq!(
            (refusal.reason, refusal.hop),
            ("private_address", 0),
            "{raw}"
        );
        assert!(
            refusal.message.starts_with("fetch_url refused: ")
                && refusal.message.contains("resolves to a private address")
                && refusal.message.contains("Nothing was sent"),
            "{refusal}"
        );
    }
    assert_eq!(site.hits.load(Ordering::SeqCst), 0, "nothing was sent");

    // The operator's say-so reaches the same pages, and a name that resolves
    // to loopback too.
    let allowed = FetchGuard::open(true);
    for raw in [format!("{base}/ok"), format!("http://localhost:{port}/ok")] {
        let page = fetch_guarded(&json!({"url": raw}), &allowed).await.unwrap();
        assert_eq!(ok(&page), (json!(200), json!("hello")));
    }
    assert_eq!(site.hits.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_name_that_looks_public_is_refused_by_the_address_it_resolves_to() {
    let (site, _) = site().await;
    let port = site.port;
    let guard = open_guard(false, &site);

    // The model is told the host, never the address; the record has both.
    let rebound = json!({"url": format!("http://rebound.example.test:{port}/x")});
    let error = fetch_guarded(&rebound, &guard).await.unwrap_err();
    let refusal = refusal_of(&error);
    assert_eq!(refusal.reason, "private_address");
    assert!(
        refusal
            .message
            .contains("rebound.example.test resolves to a private address")
            && !refusal.message.contains("127.0.0.1"),
        "{refusal}"
    );
    assert_eq!(refusal.address, Some("127.0.0.1".parse().unwrap()));
    let event = refusal.event("as_1", &rebound);
    assert_eq!(event["reason"], "private_address");
    assert_eq!(event["address"], "127.0.0.1");
    assert_eq!(refusal.reply()["refused"], true);

    // One private address among public ones is enough to refuse the name.
    let mixed = json!({"url": format!("http://mixed.example.test:{port}/x")});
    let error = fetch_guarded(&mixed, &guard).await.unwrap_err();
    assert_eq!(refusal_of(&error).reason, "private_address");
    assert_eq!(site.hits.load(Ordering::SeqCst), 0, "nothing was sent");

    // The public name goes out, so the tests below mean what they say.
    let public = json!({"url": format!("http://docs.example.test:{port}/ok")});
    let page = fetch_guarded(&public, &guard).await.unwrap();
    assert_eq!(ok(&page), (json!(200), json!("hello")));
    assert_eq!(site.hits.load(Ordering::SeqCst), 1);
    // And with the operator's say-so, so does the name that answers loopback.
    let page = fetch_guarded(&rebound, &open_guard(true, &site))
        .await
        .unwrap();
    assert_eq!(page["status"], 200);
}

#[tokio::test]
async fn a_redirect_from_a_public_host_to_a_private_one_is_refused_at_that_hop() {
    let (site, _) = site().await;
    let port = site.port;
    let guard = open_guard(false, &site);
    for (path, refused_url, host) in [
        (
            "to-loopback",
            format!("http://127.0.0.1:{port}/secret"),
            "127.0.0.1",
        ),
        (
            "to-metadata",
            "http://169.254.169.254/latest/meta-data/".to_owned(),
            "169.254.169.254",
        ),
        (
            "to-rebound",
            format!("http://rebound.example.test:{port}/secret"),
            "rebound.example.test",
        ),
    ] {
        let before = site.hits.load(Ordering::SeqCst);
        // What a keyed fetch puts its key on: each request that is sent.
        let sent = std::sync::Mutex::new(Vec::new());
        let start = format!("http://docs.example.test:{port}/{path}");
        let error = fetch_guarded_with(&json!({"url": start}), &guard, |request| {
            sent.lock().unwrap().push(request.url().to_string());
        })
        .await
        .unwrap_err();
        let refusal = refusal_of(&error);
        assert_eq!(refusal.reason, "private_address", "{path}");
        assert_eq!(
            (refusal.hop, refusal.url.as_str()),
            (1, refused_url.as_str()),
            "{path}"
        );
        assert!(
            refusal
                .message
                .contains(&format!("fetch_url refused: {host} resolves")),
            "{refusal}"
        );
        assert_eq!(
            site.hits.load(Ordering::SeqCst),
            before + 1,
            "{path}: the first hop went out, the redirect did not"
        );
        assert_eq!(
            *sent.lock().unwrap(),
            [start],
            "{path}: no request was built for the refused hop"
        );
    }
}

#[tokio::test]
async fn a_name_is_resolved_once_per_hop_and_the_connection_goes_to_that_answer() {
    let (site, _) = site().await;
    let port = site.port;
    // A name that answers a public address to the check and the host's
    // loopback to anyone who asks again, as a rebinding attacker's does.
    let lookups = Arc::new(AtomicUsize::new(0));
    let asked = lookups.clone();
    let guard = FetchGuard::open(false)
        .resolving(move |_| {
            if asked.fetch_add(1, Ordering::SeqCst) == 0 {
                vec!["93.184.216.34".parse().unwrap()]
            } else {
                vec!["127.0.0.1".parse().unwrap()]
            }
        })
        .dialing(SocketAddr::from(([127, 0, 0, 1], port)));
    let rebind = json!({"url": format!("http://rebind.example.test:{port}/ok")});
    let page = fetch_guarded(&rebind, &guard).await.unwrap();
    assert_eq!(page["status"], 200);
    assert_eq!(
        lookups.load(Ordering::SeqCst),
        1,
        "one lookup, which the client was pinned to"
    );

    // The next hop is a fresh lookup, judged on its own answer.
    let error = fetch_guarded(&rebind, &guard).await.unwrap_err();
    assert_eq!(refusal_of(&error).reason, "private_address");
    assert_eq!(lookups.load(Ordering::SeqCst), 2);
}

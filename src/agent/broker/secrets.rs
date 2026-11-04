use super::registry::Cap;
use super::{AgentBroker, Refusal, Reply, WorkerSeat, failed, forbidden, refuse};
use crate::agent::environment::LOCAL;
use crate::agent::ports::Fact;
use crate::broker::fetch_with;
use crate::vault::{Admission, GrantState, ItemKind, SecretName, UseContext, policy};
use axum::{Json, http::StatusCode};
use serde_json::{Value, json};
use solos_agent_core::model::{SecretGrant, SecretId, Ticket};
use std::collections::BTreeSet;

const NOTE: &str = "You never see these values. `use` names the tools that take each item. Pass \
                    a key's name as fetch_url's key, and the broker adds the key only to \
                    requests to its hosts. Pass a login's name to browser_login, and the browser \
                    fills it only on the login's own site. A copy of a value in a response reads \
                    [vault:<name>]. approval is where this ticket stands: none needs no person, \
                    needed asks a person at first use, waiting means one was asked, granted or \
                    denied is their answer, and no_approver means nobody can answer from this \
                    ticket's channels, so the item cannot be used.";

pub(super) async fn fetch(
    broker: &AgentBroker,
    seat: &WorkerSeat,
    key: Value,
    args: &Value,
) -> Result<Value, Refusal> {
    let name: SecretName = serde_json::from_value(key).map_err(|err| {
        refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("bad arguments: key: {err}"),
        )
    })?;
    let parts = broker
        .vault()
        .ok_or_else(|| forbidden(Cap::Vault.unavailable()))?;
    let url = args["url"]
        .as_str()
        .and_then(|url| reqwest::Url::parse(url).ok())
        .ok_or_else(|| {
            refuse(
                StatusCode::UNPROCESSABLE_ENTITY,
                "fetch_url's url is not an absolute URL",
            )
        })?;
    let context = use_context(broker, seat).await?;
    let key = match parts
        .vault
        .admit_key(&name, &url, &context)
        .map_err(failed)?
    {
        Admission::Allowed(key) => key,
        Admission::NeedsApproval {
            secret,
            label,
            answerable_by,
        } => {
            let asked = context.grants.get(&name) == Some(&GrantState::Waiting);
            return Err(ask_first_use(broker, seat, secret, label, answerable_by, asked).await);
        }
        Admission::Refused(why) => return Err(forbidden(why)),
    };
    let used = Fact::KeyUsed {
        ticket: seat.ticket.clone(),
        assignment: seat.assignment.clone(),
        secret: SecretId::try_from(format!("{}/{name}", context.environment)).map_err(failed)?,
        method: args["method"]
            .as_str()
            .unwrap_or("GET")
            .to_ascii_uppercase(),
        url: url.to_string(),
    };
    if let Err(why) = broker.inner.facts.ask(used).await.map_err(failed)? {
        return Err(refuse(StatusCode::CONFLICT, why));
    }
    let sent = fetch_with(&broker.inner.keyed, args, |request| {
        key.reveal_into(request)
    })
    .await;
    let scrubber = parts.vault.scrubber().map_err(failed)?;
    match sent {
        Ok(mut response) => {
            scrubber.clean_json(&mut response);
            Ok(response)
        }
        Err(err) => Err(refuse(
            StatusCode::BAD_GATEWAY,
            scrubber.clean(&format!("{err:#}")),
        )),
    }
}

pub(super) async fn vault_list(broker: &AgentBroker, seat: &WorkerSeat) -> Reply {
    let parts = broker
        .vault()
        .ok_or_else(|| failed("vault_list was admitted without a vault"))?;
    let context = use_context(broker, seat).await?;
    let offered = broker.seat_tools(seat);
    let login_use: &[&str] = if offered.contains(&"browser_login") {
        &["browser_login"]
    } else {
        &[]
    };
    let (mut logins, mut keys) = (Vec::new(), Vec::new());
    for item in parts.vault.catalog(&context).map_err(failed)? {
        let approval = policy::standing(&item, &context);
        match item.kind {
            ItemKind::Login {
                sites,
                username,
                hint,
            } => logins.push(json!({
                "login": item.name,
                "sites": sites,
                "username": username,
                "hint": hint,
                "where": LOCAL,
                "use": login_use,
                "approval": approval,
            })),
            ItemKind::Key { hosts, about, .. } => keys.push(json!({
                "key": item.name,
                "hosts": hosts,
                "about": about,
                "where": LOCAL,
                "use": ["fetch_url"],
                "approval": approval,
            })),
        }
    }
    Ok(Json(json!({"logins": logins, "keys": keys, "note": NOTE})))
}

pub(super) async fn use_context(
    broker: &AgentBroker,
    seat: &WorkerSeat,
) -> Result<UseContext, Refusal> {
    let id = seat.ticket.clone();
    let ticket = broker
        .store(move |store| store.read_ticket(&id))
        .await
        .map_err(failed)?
        .ok_or_else(|| failed(format!("{} is not in the store", seat.ticket)))?;
    let here = format!("{LOCAL}/");
    let grants = ticket
        .secrets
        .iter()
        .filter_map(|(secret, grant)| {
            let name = secret.as_str().strip_prefix(&here)?.parse().ok()?;
            let state = match grant {
                SecretGrant::Waiting { .. } => GrantState::Waiting,
                SecretGrant::Granted { .. } => GrantState::Granted,
                SecretGrant::Denied { .. } => GrantState::Denied,
            };
            Some((name, state))
        })
        .collect();
    let takes_approvals = broker
        .vault()
        .is_some_and(|parts| can_ask(&ticket, &parts.asks_on));
    let mandate = &ticket.mandate;
    Ok(UseContext {
        requester: format!("{}:{}", mandate.source, mandate.requester.id),
        environment: LOCAL.to_owned(),
        grants,
        takes_approvals,
    })
}

fn can_ask(ticket: &Ticket, asks_on: &BTreeSet<String>) -> bool {
    ticket
        .channels
        .iter()
        .any(|channel| asks_on.contains(&channel.source))
}

pub(super) async fn ask_first_use(
    broker: &AgentBroker,
    seat: &WorkerSeat,
    secret: String,
    label: String,
    answerable_by: Vec<String>,
    asked_before: bool,
) -> Refusal {
    let secret = match SecretId::try_from(secret) {
        Ok(secret) => secret,
        Err(why) => return failed(why),
    };
    let fact = Fact::SecretAsk {
        ticket: seat.ticket.clone(),
        secret,
        label: label.clone(),
        answerable_by,
    };
    match broker.inner.facts.ask(fact).await {
        Err(err) => failed(err),
        Ok(Err(why)) => refuse(StatusCode::CONFLICT, why),
        Ok(Ok(_)) => {
            let ask = if asked_before {
                format!("A person has not yet allowed this ticket to {label}")
            } else {
                format!("A person must allow this ticket to {label} first")
            };
            refuse(
                StatusCode::CONFLICT,
                format!(
                    "{ask}; they were asked in the thread. End your turn: their answer comes \
                     back as new input."
                ),
            )
        }
    }
}

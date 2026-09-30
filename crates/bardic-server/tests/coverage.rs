//! Tracks which contract operations the server implements, so a contract change
//! cannot silently add work nobody triaged.
mod common;
use common::Contract;

/// Operation ids with a handler and a conformance test. Move ids here as milestones land.
const IMPLEMENTED: &[&str] = &[
    "getHealth",
    "getServer",
    "updateServer",
    "streamEvents",
    "listDevices",
    "updateDevice",
    "listAudit",
    "listListeners",
    "createListener",
    "getListener",
    "renameListener",
    "deleteListener",
    "getListenerImpact",
    "getListenerSettings",
    "putListenerSettings",
];

#[test]
fn implemented_operations_exist_in_the_contract() {
    let c = Contract::load();
    let ids = c.operation_ids();
    for id in IMPLEMENTED {
        assert!(ids.contains(&id.to_string()), "{id} is not in the contract");
    }
    eprintln!(
        "implemented {} of {} operations",
        IMPLEMENTED.len(),
        ids.len()
    );
}

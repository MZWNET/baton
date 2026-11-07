use uuid::Uuid;

use crate::Agent;

// Fixed so that every machine derives the same id for the same session
const NAMESPACE: Uuid = Uuid::from_u128(0x7a1c3e52_9b04_4d8f_a6e1_5c2b8f0d4e93);

// A session created by importing a native session
pub(super) fn imported(agent: Agent, native_id: &str) -> String {
    derive(&format!("{agent}:{native_id}"))
}

// A session the native session continued into from `base` of `parent`
pub(super) fn forked(parent: &str, base: Option<&str>, agent: Agent, native_id: &str) -> String {
    derive(&format!(
        "fork:{parent}:{}:{agent}:{native_id}",
        base.unwrap_or_default()
    ))
}

// An old-layout session that no native session identifies
pub(super) fn legacy(old: &str) -> String {
    derive(&format!("legacy:{old}"))
}

pub(super) fn exported(session: &str, host: &str, agent: Agent, leaf: Option<&str>) -> Uuid {
    uuid(&format!(
        "export:{session}:{host}:{agent}:{}",
        leaf.unwrap_or_default()
    ))
}

fn derive(name: &str) -> String {
    uuid(name).to_string()
}

fn uuid(name: &str) -> Uuid {
    Uuid::new_v5(&NAMESPACE, name.as_bytes())
}

use baton_core::{Body, Event, ToolResult, User};

// One native message per run of replayed events, the way chat transcripts group them
pub(crate) enum Unit<'a> {
    User(&'a Event, &'a User),
    // Text blocks and tool calls the assistant produced in one turn
    Assistant(Vec<&'a Event>),
    ToolResult(&'a Event, &'a ToolResult),
    Compaction(&'a Event, &'a str),
}

pub(crate) fn units<'a>(events: &[&'a Event]) -> Vec<Unit<'a>> {
    let mut units = Vec::new();
    for &event in events {
        match &event.body {
            Body::User(user) => units.push(Unit::User(event, user)),
            Body::Assistant(_) | Body::ToolCall(_) => match units.last_mut() {
                Some(Unit::Assistant(run)) => run.push(event),
                _ => units.push(Unit::Assistant(vec![event])),
            },
            Body::ToolResult(result) => units.push(Unit::ToolResult(event, result)),
            Body::Compaction(compaction) => {
                units.push(Unit::Compaction(event, &compaction.summary))
            }
            Body::Thinking(_) | Body::Meta(_) => {}
        }
    }
    units
}

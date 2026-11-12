use baton_core::replay::Model;

// What baton writes for a provider or model the session never named, the marker Claude Code uses for replies no model produced
pub(crate) const UNKNOWN: &str = "<synthetic>";
// Older exports used this marker, which parsers skip to preserve their digests
pub(crate) const LEGACY_PLACEHOLDER: &str = "baton";

pub(crate) fn provider_and_id(model: Option<Model<'_>>) -> (&str, &str) {
    let (provider, id) = model.map_or((None, UNKNOWN), |model| model.split());

    (provider.unwrap_or(UNKNOWN), id)
}

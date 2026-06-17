use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use std::collections::BTreeSet;

use super::context_packets::{
    get_packet_summary, get_retrieval_event, list_visible_packet_items, ContextPacketItemSummary,
    ContextPacketSummary, RetrievalEventSummary,
};

const MAX_HANDOFF_PACKET_IDS: usize = 8;
const MAX_HANDOFF_RETRIEVAL_IDS: usize = 8;
const MAX_HANDOFF_ITEMS_PER_PACKET: usize = 8;

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct ContextHandoffRequest {
    #[serde(default)]
    pub context_packet_ids: Vec<String>,
    #[serde(default)]
    pub retrieval_ids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResolvedContextHandoff {
    pub packets: Vec<ResolvedContextPacket>,
    pub retrievals: Vec<RetrievalEventSummary>,
}

#[derive(Debug, Serialize)]
pub(crate) struct ResolvedContextPacket {
    #[serde(flatten)]
    pub summary: ContextPacketSummary,
    pub items: Vec<ContextPacketItemSummary>,
}

impl ContextHandoffRequest {
    pub(crate) fn has_handles(&self) -> bool {
        self.context_packet_ids
            .iter()
            .any(|id| !id.trim().is_empty())
            || self.retrieval_ids.iter().any(|id| !id.trim().is_empty())
    }
}

impl ResolvedContextHandoff {
    pub(crate) fn packet_ids(&self) -> Vec<String> {
        self.packets
            .iter()
            .map(|packet| packet.summary.packet_id.clone())
            .collect()
    }

    pub(crate) fn retrieval_ids(&self) -> Vec<String> {
        self.retrievals
            .iter()
            .map(|retrieval| retrieval.retrieval_id.clone())
            .collect()
    }

    pub(crate) fn prompt_json(&self) -> Result<String> {
        serde_json::to_string(self).context("serialize artifact-memory handoff")
    }

    pub(crate) fn metadata_json(&self) -> Value {
        json!({
            "context_packet_ids": self.packet_ids(),
            "retrieval_ids": self.retrieval_ids(),
            "packet_count": self.packets.len(),
            "retrieval_count": self.retrievals.len(),
        })
    }
}

pub(crate) async fn resolve_context_handoff(
    pool: &SqlitePool,
    organization: &str,
    request: &ContextHandoffRequest,
) -> Result<Option<ResolvedContextHandoff>> {
    let packet_ids = normalize_handles(&request.context_packet_ids, MAX_HANDOFF_PACKET_IDS)
        .context("normalize context_packet_ids")?;
    let requested_retrieval_ids =
        normalize_handles(&request.retrieval_ids, MAX_HANDOFF_RETRIEVAL_IDS)
            .context("normalize retrieval_ids")?;

    if packet_ids.is_empty() && requested_retrieval_ids.is_empty() {
        return Ok(None);
    }

    let mut packets = Vec::with_capacity(packet_ids.len());
    let mut retrieval_ids = requested_retrieval_ids;
    for packet_id in packet_ids {
        let summary = get_packet_summary(pool, organization, &packet_id)
            .await
            .with_context(|| format!("load context packet {packet_id}"))?
            .with_context(|| format!("context packet not found or not visible: {packet_id}"))?;
        if let Some(retrieval_id) = summary.retrieval_id.as_deref() {
            push_unique(&mut retrieval_ids, retrieval_id.to_string());
        }

        let mut items = list_visible_packet_items(pool, organization, &packet_id)
            .await
            .with_context(|| format!("load context packet items {packet_id}"))?;
        if items.len() > MAX_HANDOFF_ITEMS_PER_PACKET {
            items.truncate(MAX_HANDOFF_ITEMS_PER_PACKET);
        }
        packets.push(ResolvedContextPacket { summary, items });
    }

    if retrieval_ids.len() > MAX_HANDOFF_RETRIEVAL_IDS {
        bail!(
            "too many retrieval_ids after packet expansion: {} > {}",
            retrieval_ids.len(),
            MAX_HANDOFF_RETRIEVAL_IDS
        );
    }

    let mut retrievals = Vec::with_capacity(retrieval_ids.len());
    for retrieval_id in retrieval_ids {
        let retrieval = get_retrieval_event(pool, organization, &retrieval_id)
            .await
            .with_context(|| format!("load retrieval trace {retrieval_id}"))?
            .with_context(|| format!("retrieval trace not found or not visible: {retrieval_id}"))?;
        retrievals.push(retrieval);
    }

    Ok(Some(ResolvedContextHandoff {
        packets,
        retrievals,
    }))
}

fn normalize_handles(raw: &[String], max: usize) -> Result<Vec<String>> {
    let mut seen = BTreeSet::new();
    let mut values = Vec::new();
    for value in raw {
        let trimmed = value.trim();
        if trimmed.is_empty() || !seen.insert(trimmed.to_string()) {
            continue;
        }
        values.push(trimmed.to_string());
    }
    if values.len() > max {
        bail!("too many handles: {} > {}", values.len(), max);
    }
    Ok(values)
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

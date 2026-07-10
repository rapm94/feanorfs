use anyhow::Result;

use super::ClientDb;
use crate::state::AccessEntryV1;

impl ClientDb {
    pub async fn record_access_pair(
        &self,
        path: &str,
        sibling: &str,
        weight_delta: f64,
    ) -> Result<()> {
        if !weight_delta.is_finite() {
            anyhow::bail!("non-finite weight delta {weight_delta} for {path}/{sibling}");
        }
        let now = chrono::Utc::now().timestamp_millis();
        let path = path.to_string();
        let sibling = sibling.to_string();
        self.state.with_write(|state| {
            if let Some(existing) = state
                .file_access_log
                .iter_mut()
                .find(|entry| entry.path == path && entry.sibling_path == sibling)
            {
                let new_weight = existing.weight + weight_delta;
                if !new_weight.is_finite() {
                    anyhow::bail!(
                        "overflow: weight {} + delta {} overflows for {}/{}",
                        existing.weight,
                        weight_delta,
                        path,
                        sibling
                    );
                }
                existing.weight = new_weight;
                existing.updated_at = now;
            } else {
                state.file_access_log.push(AccessEntryV1 {
                    path: path.clone(),
                    sibling_path: sibling.clone(),
                    weight: weight_delta,
                    updated_at: now,
                });
            }
            state.prune_access_log();
            Ok(())
        })
    }

    pub async fn get_predictive_siblings(
        &self,
        path: &str,
        limit: usize,
    ) -> Result<Vec<(String, f64)>> {
        let path = path.to_string();
        self.state.with_read(|state| {
            let mut entries = state
                .file_access_log
                .iter()
                .filter(|entry| entry.path == path && entry.sibling_path != path)
                .collect::<Vec<_>>();
            entries.sort_by(|left, right| {
                right
                    .weight
                    .partial_cmp(&left.weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            Ok(entries
                .into_iter()
                .take(limit)
                .map(|entry| (entry.sibling_path.clone(), entry.weight))
                .collect())
        })
    }

    pub async fn decay_access_log(&self, factor: f64) -> Result<()> {
        if !factor.is_finite() {
            anyhow::bail!("non-finite decay factor {factor}");
        }
        self.state.with_write(|state| {
            for entry in &mut state.file_access_log {
                entry.weight *= factor;
            }
            state.prune_access_log();
            Ok(())
        })
    }

    pub async fn set_session_key(&self, key: &str, value: &str) -> Result<()> {
        let key = key.to_string();
        let value = value.to_string();
        self.state.with_write(|state| {
            state.last_session.insert(key, value);
            Ok(())
        })
    }

    pub async fn get_session_key(&self, key: &str) -> Result<Option<String>> {
        let key = key.to_string();
        self.state
            .with_read(|state| Ok(state.last_session.get(&key).cloned()))
    }
}

//! Durable compressed trajectories for Mèche 0.50 forward-test sessions.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

pub const TRAJECTORY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone)]
pub struct TrajectoryMetadata {
    pub session_id: String,
    pub market_slot: String,
    pub entry_time_ms: i64,
    pub slug: String,
    pub signal_ids: Vec<String>,
    pub completion_status: String,
    pub gap_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrajectoryIndexRecord {
    pub schema_version: u32,
    pub session_id: String,
    pub market_slot: String,
    pub entry_time_ms: i64,
    pub slug: String,
    pub signal_ids: Vec<String>,
    pub completion_status: String,
    pub gap_count: u64,
    pub path: String,
    pub sha256: String,
    pub observation_count: u64,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
    pub finalized_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrajectoryVerification {
    pub observation_count: u64,
    pub uncompressed_bytes: u64,
    pub compressed_bytes: u64,
}

pub fn trajectory_path(root: &Path, entry_time_ms: i64, session_id: &str) -> PathBuf {
    let date = DateTime::<Utc>::from_timestamp_millis(entry_time_ms)
        .unwrap_or_else(Utc::now)
        .format("%Y-%m-%d")
        .to_string();
    root.join("trajectories")
        .join(date)
        .join(format!("{session_id}.jsonl.zst"))
}

pub async fn finalize_trajectory(
    source: PathBuf,
    destination: PathBuf,
    metadata: TrajectoryMetadata,
) -> Result<TrajectoryIndexRecord> {
    tokio::task::spawn_blocking(move || finalize_trajectory_sync(&source, &destination, metadata))
        .await
        .context("tâche de compression de trajectoire interrompue")?
}

pub fn upsert_trajectory_index(root: &Path, record: TrajectoryIndexRecord) -> Result<()> {
    let path = root.join("trajectory_index.jsonl");
    let mut by_session = load_trajectory_index(&path)?
        .into_iter()
        .map(|existing| (existing.session_id.clone(), existing))
        .collect::<BTreeMap<_, _>>();
    by_session.insert(record.session_id.clone(), record);

    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("index de trajectoire sans dossier parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = temporary_path(&path);
    let mut writer = BufWriter::new(
        fs::File::create(&temporary)
            .with_context(|| format!("création index temporaire {}", temporary.display()))?,
    );
    for value in by_session.into_values() {
        serde_json::to_writer(&mut writer, &value)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    replace_file(&temporary, &path)
}

pub fn load_trajectory_index(path: &Path) -> Result<Vec<TrajectoryIndexRecord>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path)
        .with_context(|| format!("lecture index trajectoires {}", path.display()))?;
    let mut records = Vec::new();
    for (line_number, line) in BufReader::new(file).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        records.push(serde_json::from_str(&line).with_context(|| {
            format!(
                "index trajectoires invalide {} ligne {}",
                path.display(),
                line_number + 1
            )
        })?);
    }
    Ok(records)
}

pub fn open_trajectory_reader(path: &Path) -> Result<Box<dyn BufRead>> {
    let file =
        fs::File::open(path).with_context(|| format!("lecture trajectoire {}", path.display()))?;
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains(".zst"))
    {
        let decoder = zstd::stream::read::Decoder::new(file)
            .with_context(|| format!("décompression trajectoire {}", path.display()))?;
        Ok(Box::new(BufReader::new(decoder)))
    } else {
        Ok(Box::new(BufReader::new(file)))
    }
}

pub fn verify_trajectory(record: &TrajectoryIndexRecord) -> Result<TrajectoryVerification> {
    let path = Path::new(&record.path);
    let compressed_bytes = fs::metadata(path)
        .with_context(|| format!("métadonnées trajectoire {}", path.display()))?
        .len();
    if compressed_bytes != record.compressed_bytes {
        return Err(anyhow!(
            "taille trajectoire inattendue {}: index={} disque={}",
            path.display(),
            record.compressed_bytes,
            compressed_bytes
        ));
    }
    let sha256 = sha256_file(path)?;
    if sha256 != record.sha256 {
        return Err(anyhow!("checksum trajectoire invalide {}", path.display()));
    }
    let (observation_count, uncompressed_bytes) =
        inspect_uncompressed(open_trajectory_reader(path)?)?;
    if observation_count != record.observation_count {
        return Err(anyhow!(
            "nombre d'observations inattendu {}: index={} flux={}",
            path.display(),
            record.observation_count,
            observation_count
        ));
    }
    if uncompressed_bytes != record.uncompressed_bytes {
        return Err(anyhow!(
            "taille décompressée inattendue {}: index={} flux={}",
            path.display(),
            record.uncompressed_bytes,
            uncompressed_bytes
        ));
    }
    Ok(TrajectoryVerification {
        observation_count,
        uncompressed_bytes,
        compressed_bytes,
    })
}

pub fn recover_trajectory_index_record(
    path: &Path,
    metadata: TrajectoryMetadata,
) -> Result<TrajectoryIndexRecord> {
    let compressed_bytes = fs::metadata(path)
        .with_context(|| format!("métadonnées trajectoire {}", path.display()))?
        .len();
    let (observation_count, uncompressed_bytes) =
        inspect_uncompressed(open_trajectory_reader(path)?)?;
    if observation_count == 0 {
        return Err(anyhow!("trajectoire vide: {}", path.display()));
    }
    let finalized_at = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(DateTime::<Utc>::from)
        .unwrap_or_else(|_| Utc::now());
    Ok(TrajectoryIndexRecord {
        schema_version: TRAJECTORY_SCHEMA_VERSION,
        session_id: metadata.session_id,
        market_slot: metadata.market_slot,
        entry_time_ms: metadata.entry_time_ms,
        slug: metadata.slug,
        signal_ids: metadata.signal_ids,
        completion_status: metadata.completion_status,
        gap_count: metadata.gap_count,
        path: path.to_string_lossy().into_owned(),
        sha256: sha256_file(path)?,
        observation_count,
        uncompressed_bytes,
        compressed_bytes,
        finalized_at,
    })
}

pub fn finalize_trajectory_sync(
    source: &Path,
    destination: &Path,
    metadata: TrajectoryMetadata,
) -> Result<TrajectoryIndexRecord> {
    let uncompressed_bytes = fs::metadata(source)
        .with_context(|| format!("métadonnées stream source {}", source.display()))?
        .len();
    let observation_count = count_non_empty_lines(BufReader::new(
        fs::File::open(source)
            .with_context(|| format!("lecture stream source {}", source.display()))?,
    ))?;
    if observation_count == 0 {
        return Err(anyhow!("stream source vide: {}", source.display()));
    }

    let parent = destination
        .parent()
        .ok_or_else(|| anyhow!("trajectoire sans dossier parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = temporary_path(destination);
    let mut input = BufReader::new(fs::File::open(source)?);
    let output = fs::File::create(&temporary)
        .with_context(|| format!("création trajectoire temporaire {}", temporary.display()))?;
    let mut encoder = zstd::stream::write::Encoder::new(BufWriter::new(output), 3)?;
    std::io::copy(&mut input, &mut encoder)?;
    let mut writer = encoder.finish()?;
    writer.flush()?;
    writer.get_ref().sync_all()?;

    let verified_count = count_non_empty_lines(open_trajectory_reader(&temporary)?)?;
    if verified_count != observation_count {
        return Err(anyhow!(
            "validation trajectoire échouée: source={} compressé={}",
            observation_count,
            verified_count
        ));
    }
    replace_file(&temporary, destination)?;
    let compressed_bytes = fs::metadata(destination)?.len();
    let sha256 = sha256_file(destination)?;

    Ok(TrajectoryIndexRecord {
        schema_version: TRAJECTORY_SCHEMA_VERSION,
        session_id: metadata.session_id,
        market_slot: metadata.market_slot,
        entry_time_ms: metadata.entry_time_ms,
        slug: metadata.slug,
        signal_ids: metadata.signal_ids,
        completion_status: metadata.completion_status,
        gap_count: metadata.gap_count,
        path: destination.to_string_lossy().into_owned(),
        sha256,
        observation_count,
        uncompressed_bytes,
        compressed_bytes,
        finalized_at: Utc::now(),
    })
}

fn count_non_empty_lines(mut reader: impl BufRead) -> Result<u64> {
    Ok(inspect_uncompressed(&mut reader)?.0)
}

fn inspect_uncompressed(mut reader: impl BufRead) -> Result<(u64, u64)> {
    let mut count = 0_u64;
    let mut total_bytes = 0_u64;
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        let read_bytes = reader.read_until(b'\n', &mut buffer)?;
        if read_bytes == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(u64::try_from(read_bytes).unwrap_or(u64::MAX));
        if buffer.iter().any(|byte| !byte.is_ascii_whitespace()) {
            count += 1;
        }
    }
    Ok((count, total_bytes))
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            break;
        }
        hasher.update(&buffer[..bytes]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn temporary_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.tmp", path.to_string_lossy()))
}

fn replace_file(temporary: &Path, destination: &Path) -> Result<()> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)?;
    }
    fs::rename(temporary, destination).with_context(|| {
        format!(
            "finalisation atomique {} -> {}",
            temporary.display(),
            destination.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        finalize_trajectory, load_trajectory_index, trajectory_path, upsert_trajectory_index,
        verify_trajectory, TrajectoryMetadata,
    };
    use chrono::Utc;
    use std::fs;

    fn temporary_root(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "meche050-trajectory-{name}-{}-{}",
            std::process::id(),
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn metadata() -> TrajectoryMetadata {
        TrajectoryMetadata {
            session_id: "session-1".to_string(),
            market_slot: "btc_5m".to_string(),
            entry_time_ms: 1_700_000_000_000,
            slug: "btc-updown-5m-1700000000".to_string(),
            signal_ids: vec!["signal-1".to_string()],
            completion_status: "RESOLVED_COMPLETE".to_string(),
            gap_count: 0,
        }
    }

    #[tokio::test]
    async fn finalized_trajectory_round_trips_and_verifies() {
        let root = temporary_root("roundtrip");
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.jsonl");
        fs::write(&source, b"{\"sequence\":1}\n{\"sequence\":2}\n").unwrap();
        let destination = trajectory_path(&root, metadata().entry_time_ms, "session-1");

        let record = finalize_trajectory(source, destination, metadata())
            .await
            .unwrap();
        let verification = verify_trajectory(&record).unwrap();

        assert_eq!(verification.observation_count, 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn trajectory_index_upsert_is_idempotent_by_session() {
        let root = temporary_root("index");
        fs::create_dir_all(&root).unwrap();
        let mut record = super::TrajectoryIndexRecord {
            schema_version: 1,
            session_id: "session-1".to_string(),
            market_slot: "btc_5m".to_string(),
            entry_time_ms: 1,
            slug: "slug".to_string(),
            signal_ids: Vec::new(),
            completion_status: "RESOLVED_COMPLETE".to_string(),
            gap_count: 0,
            path: "first".to_string(),
            sha256: "hash".to_string(),
            observation_count: 1,
            uncompressed_bytes: 1,
            compressed_bytes: 1,
            finalized_at: Utc::now(),
        };
        upsert_trajectory_index(&root, record.clone()).unwrap();
        record.path = "second".to_string();
        upsert_trajectory_index(&root, record).unwrap();

        let records = load_trajectory_index(&root.join("trajectory_index.jsonl")).unwrap();

        assert_eq!(records[0].path, "second");
        fs::remove_dir_all(root).unwrap();
    }
}

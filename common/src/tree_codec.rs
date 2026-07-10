use crate::{is_valid_hash, Snapshot, Tree, TreeEntry, TreeEntryKind, EXECUTABLE_MODE};
use anyhow::{bail, Context, Result};

const TREE_MAGIC: &[u8; 4] = b"FTR1";
const SNAPSHOT_MAGIC: &[u8; 4] = b"FSN1";

pub(crate) fn encode_tree(tree: &Tree) -> Vec<u8> {
    let mut entries: Vec<_> = tree.entries.iter().collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let mut out = Vec::new();
    out.extend_from_slice(TREE_MAGIC);
    push_len(&mut out, entries.len());
    for entry in entries {
        push_string(&mut out, &entry.name);
        match &entry.kind {
            TreeEntryKind::File => out.push(0),
            TreeEntryKind::Dir => out.push(1),
            TreeEntryKind::Conflict { base, ours, theirs } => {
                out.push(2);
                push_option_string(&mut out, base.as_deref());
                push_option_string(&mut out, ours.as_deref());
                push_option_string(&mut out, theirs.as_deref());
            }
        }
        push_string(&mut out, &entry.hash);
        out.extend_from_slice(&entry.size.to_le_bytes());
        out.extend_from_slice(&entry.mode.to_le_bytes());
    }
    out
}

pub(crate) fn decode_tree(bytes: &[u8]) -> Result<Tree> {
    let mut decoder = Decoder::new(bytes);
    decoder.expect_magic(TREE_MAGIC)?;
    let count = decoder.read_len()?;
    let mut entries = Vec::new();
    for _ in 0..count {
        let name = decoder.read_string()?;
        let tag = decoder.read_u8()?;
        let kind = match tag {
            0 => TreeEntryKind::File,
            1 => TreeEntryKind::Dir,
            2 => TreeEntryKind::Conflict {
                base: decoder.read_option_string()?,
                ours: decoder.read_option_string()?,
                theirs: decoder.read_option_string()?,
            },
            other => bail!("unknown tree entry kind {other}"),
        };
        let hash = decoder.read_string()?;
        let size = decoder.read_u64()?;
        let mode = decoder.read_u32()?;
        validate_entry(&name, &kind, &hash, mode)?;
        entries.push(TreeEntry {
            name,
            kind,
            hash,
            size,
            mode,
        });
    }
    decoder.finish()?;
    ensure_sorted_unique(&entries)?;
    Ok(Tree { entries })
}

pub(crate) fn encode_snapshot(snapshot: &Snapshot) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(SNAPSHOT_MAGIC);
    push_string(&mut out, &snapshot.root);
    push_len(&mut out, snapshot.parents.len());
    for parent in &snapshot.parents {
        push_string(&mut out, parent);
    }
    push_string(&mut out, &snapshot.author);
    out.extend_from_slice(&snapshot.created_at_ms.to_le_bytes());
    push_option_string(&mut out, snapshot.message.as_deref());
    out
}

pub(crate) fn decode_snapshot(bytes: &[u8]) -> Result<Snapshot> {
    let mut decoder = Decoder::new(bytes);
    decoder.expect_magic(SNAPSHOT_MAGIC)?;
    let root = decoder.read_string()?;
    let parent_count = decoder.read_len()?;
    let mut parents = Vec::new();
    for _ in 0..parent_count {
        parents.push(decoder.read_string()?);
    }
    let author = decoder.read_string()?;
    let created_at_ms = decoder.read_i64()?;
    let message = decoder.read_option_string()?;
    decoder.finish()?;
    if !is_valid_hash(&root) || parents.iter().any(|parent| !is_valid_hash(parent)) {
        bail!("snapshot contains an invalid object id");
    }
    Ok(Snapshot {
        root,
        parents,
        author,
        created_at_ms,
        message,
    })
}

fn validate_entry(name: &str, kind: &TreeEntryKind, hash: &str, mode: u32) -> Result<()> {
    if name.is_empty() || name.contains('/') || name.contains('\\') || name == "." || name == ".." {
        bail!("invalid tree entry name {name:?}");
    }
    if mode != 0 && mode != EXECUTABLE_MODE {
        bail!("invalid portable mode {mode}");
    }
    if !matches!(kind, TreeEntryKind::File) && mode != 0 {
        bail!("only file entries may be executable");
    }
    if !is_valid_hash(hash) {
        bail!("tree entry contains an invalid object id");
    }
    if let TreeEntryKind::Conflict { base, ours, theirs } = kind {
        if [base, ours, theirs]
            .into_iter()
            .flatten()
            .any(|leg| !is_valid_hash(leg))
        {
            bail!("conflict contains an invalid leg id");
        }
        let visible = theirs.as_deref().or(ours.as_deref()).or(base.as_deref());
        if visible != Some(hash) {
            bail!("conflict hash must identify its visible leg");
        }
    }
    Ok(())
}

fn ensure_sorted_unique(entries: &[TreeEntry]) -> Result<()> {
    for pair in entries.windows(2) {
        if pair[0].name >= pair[1].name {
            bail!("tree entries are not canonically sorted and unique");
        }
    }
    Ok(())
}

fn push_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_le_bytes());
}

fn push_string(out: &mut Vec<u8>, value: &str) {
    push_len(out, value.len());
    out.extend_from_slice(value.as_bytes());
}

fn push_option_string(out: &mut Vec<u8>, value: Option<&str>) {
    match value {
        Some(value) => {
            out.push(1);
            push_string(out, value);
        }
        None => out.push(0),
    }
}

struct Decoder<'a> {
    remaining: &'a [u8],
}

impl<'a> Decoder<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn expect_magic(&mut self, expected: &[u8]) -> Result<()> {
        if self.take(expected.len())? != expected {
            bail!("unsupported canonical object format");
        }
        Ok(())
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        if len > self.remaining.len() {
            bail!("truncated canonical object");
        }
        let (value, rest) = self.remaining.split_at(len);
        self.remaining = rest;
        Ok(value)
    }

    fn read_u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn read_u32(&mut self) -> Result<u32> {
        let bytes: [u8; 4] = self.take(4)?.try_into().context("invalid u32")?;
        Ok(u32::from_le_bytes(bytes))
    }

    fn read_u64(&mut self) -> Result<u64> {
        let bytes: [u8; 8] = self.take(8)?.try_into().context("invalid u64")?;
        Ok(u64::from_le_bytes(bytes))
    }

    fn read_i64(&mut self) -> Result<i64> {
        let bytes: [u8; 8] = self.take(8)?.try_into().context("invalid i64")?;
        Ok(i64::from_le_bytes(bytes))
    }

    fn read_len(&mut self) -> Result<usize> {
        usize::try_from(self.read_u64()?).context("canonical length exceeds platform limits")
    }

    fn read_string(&mut self) -> Result<String> {
        let len = self.read_len()?;
        let bytes = self.take(len)?;
        Ok(std::str::from_utf8(bytes)
            .context("canonical string is not UTF-8")?
            .to_owned())
    }

    fn read_option_string(&mut self) -> Result<Option<String>> {
        match self.read_u8()? {
            0 => Ok(None),
            1 => Ok(Some(self.read_string()?)),
            other => bail!("invalid option tag {other}"),
        }
    }

    fn finish(self) -> Result<()> {
        if !self.remaining.is_empty() {
            bail!("trailing bytes in canonical object");
        }
        Ok(())
    }
}

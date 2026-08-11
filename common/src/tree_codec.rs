use crate::{
    is_safe_rel_path, is_valid_hash, ConflictModes, Snapshot, Tree, TreeEntry, TreeEntryKind,
    EXECUTABLE_MODE, MAX_CANONICAL_OBJECT_BYTES, MAX_SNAPSHOT_AUTHOR_BYTES,
    MAX_SNAPSHOT_MESSAGE_BYTES, MAX_SNAPSHOT_PARENTS, MAX_TREE_ENTRIES,
};
use anyhow::{bail, Context, Result};

const TREE_MAGIC_V1: &[u8; 4] = b"FTR1";
const TREE_MAGIC_V2: &[u8; 4] = b"FTR2";
const SNAPSHOT_MAGIC: &[u8; 4] = b"FSN1";

pub(crate) fn encode_tree(tree: &Tree) -> Vec<u8> {
    let mut entries: Vec<_> = tree.entries.iter().collect();
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let uses_v2 = tree_uses_v2(tree);
    let mut out = Vec::new();
    out.extend_from_slice(if uses_v2 {
        TREE_MAGIC_V2
    } else {
        TREE_MAGIC_V1
    });
    push_len(&mut out, entries.len());
    for entry in entries {
        push_string(&mut out, &entry.name);
        match &entry.kind {
            TreeEntryKind::File => out.push(0),
            TreeEntryKind::Dir => out.push(1),
            TreeEntryKind::Conflict {
                base,
                ours,
                theirs,
                modes,
            } => {
                out.push(2);
                push_option_string(&mut out, base.as_deref());
                push_option_string(&mut out, ours.as_deref());
                push_option_string(&mut out, theirs.as_deref());
                if uses_v2 {
                    out.extend_from_slice(&modes.base.to_le_bytes());
                    out.extend_from_slice(&modes.ours.to_le_bytes());
                    out.extend_from_slice(&modes.theirs.to_le_bytes());
                }
            }
        }
        push_string(&mut out, &entry.hash);
        out.extend_from_slice(&entry.size.to_le_bytes());
        out.extend_from_slice(&entry.mode.to_le_bytes());
    }
    out
}

pub(crate) fn decode_tree(bytes: &[u8]) -> Result<Tree> {
    if bytes.len() > MAX_CANONICAL_OBJECT_BYTES {
        bail!("canonical tree exceeds object byte limit");
    }
    let mut decoder = Decoder::new(bytes);
    let uses_v2 = if bytes.starts_with(TREE_MAGIC_V1) {
        decoder.expect_magic(TREE_MAGIC_V1)?;
        false
    } else if bytes.starts_with(TREE_MAGIC_V2) {
        decoder.expect_magic(TREE_MAGIC_V2)?;
        true
    } else {
        bail!("unsupported canonical object format");
    };
    let count = decoder.read_len()?;
    if count > MAX_TREE_ENTRIES || count > decoder.remaining_len() / 94 {
        bail!("canonical tree contains an impossible entry count");
    }
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let name = decoder.read_string()?;
        let tag = decoder.read_u8()?;
        let kind = match tag {
            0 => TreeEntryKind::File,
            1 => TreeEntryKind::Dir,
            2 => {
                let base = decoder.read_option_string()?;
                let ours = decoder.read_option_string()?;
                let theirs = decoder.read_option_string()?;
                let modes = if uses_v2 {
                    ConflictModes {
                        base: decoder.read_u32()?,
                        ours: decoder.read_u32()?,
                        theirs: decoder.read_u32()?,
                    }
                } else {
                    ConflictModes::default()
                };
                TreeEntryKind::Conflict {
                    base,
                    ours,
                    theirs,
                    modes,
                }
            }
            other => bail!("unknown tree entry kind {other}"),
        };
        let hash = decoder.read_string()?;
        let size = decoder.read_u64()?;
        let mode = decoder.read_u32()?;
        validate_entry(&name, &kind, &hash, size, mode)?;
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
    let tree = Tree { entries };
    validate_tree(&tree)?;
    if uses_v2 && !tree_uses_v2(&tree) {
        bail!("non-canonical FTR2 tree without executable conflict intent");
    }
    Ok(tree)
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
    if bytes.len() > MAX_CANONICAL_OBJECT_BYTES {
        bail!("canonical snapshot exceeds object byte limit");
    }
    let mut decoder = Decoder::new(bytes);
    decoder.expect_magic(SNAPSHOT_MAGIC)?;
    let root = decoder.read_string()?;
    let parent_count = decoder.read_len()?;
    if parent_count > MAX_SNAPSHOT_PARENTS {
        bail!("snapshot contains too many parents");
    }
    let mut parents = Vec::with_capacity(parent_count);
    for _ in 0..parent_count {
        parents.push(decoder.read_string()?);
    }
    let author = decoder.read_string()?;
    let created_at_ms = decoder.read_i64()?;
    let message = decoder.read_option_string()?;
    decoder.finish()?;
    let snapshot = Snapshot {
        root,
        parents,
        author,
        created_at_ms,
        message,
    };
    validate_snapshot(&snapshot)?;
    Ok(snapshot)
}

pub(crate) fn validate_tree(tree: &Tree) -> Result<()> {
    if tree.entries.len() > MAX_TREE_ENTRIES {
        bail!("canonical tree contains too many entries");
    }
    if canonical_tree_len(tree)? > MAX_CANONICAL_OBJECT_BYTES {
        bail!("canonical tree exceeds object byte limit");
    }
    let mut exact = std::collections::BTreeSet::new();
    let mut portable = std::collections::BTreeSet::new();
    for entry in &tree.entries {
        validate_entry(
            &entry.name,
            &entry.kind,
            &entry.hash,
            entry.size,
            entry.mode,
        )?;
        if !exact.insert(entry.name.as_str()) {
            bail!("tree entries are not unique");
        }
        if !portable.insert(entry.name.to_lowercase()) {
            bail!("tree entries collide under portable case folding");
        }
    }
    Ok(())
}

pub(crate) fn validate_snapshot(snapshot: &Snapshot) -> Result<()> {
    if snapshot.parents.len() > MAX_SNAPSHOT_PARENTS {
        bail!("snapshot contains too many parents");
    }
    if canonical_snapshot_len(snapshot)? > MAX_CANONICAL_OBJECT_BYTES {
        bail!("canonical snapshot exceeds object byte limit");
    }
    if !is_valid_hash(&snapshot.root)
        || snapshot.parents.iter().any(|parent| !is_valid_hash(parent))
    {
        bail!("snapshot contains an invalid object id");
    }
    if snapshot.author.len() > MAX_SNAPSHOT_AUTHOR_BYTES {
        bail!("snapshot author exceeds byte limit");
    }
    if snapshot
        .message
        .as_ref()
        .is_some_and(|message| message.len() > MAX_SNAPSHOT_MESSAGE_BYTES)
    {
        bail!("snapshot message exceeds byte limit");
    }
    let mut unique = std::collections::BTreeSet::new();
    if snapshot.parents.iter().any(|parent| !unique.insert(parent)) {
        bail!("snapshot contains duplicate parents");
    }
    Ok(())
}

fn tree_uses_v2(tree: &Tree) -> bool {
    tree.entries.iter().any(|entry| {
        matches!(
            &entry.kind,
            TreeEntryKind::Conflict { modes, .. } if !modes.is_zero()
        )
    })
}

fn checked_len(total: &mut usize, amount: usize) -> Result<()> {
    *total = total
        .checked_add(amount)
        .context("canonical object length overflow")?;
    Ok(())
}

fn option_string_len(value: Option<&str>) -> Result<usize> {
    match value {
        Some(value) => 1_usize
            .checked_add(8)
            .and_then(|length| length.checked_add(value.len()))
            .context("canonical option length overflow"),
        None => Ok(1),
    }
}

fn canonical_tree_len(tree: &Tree) -> Result<usize> {
    let mut length = 4 + 8;
    let uses_v2 = tree_uses_v2(tree);
    for entry in &tree.entries {
        checked_len(&mut length, 8 + entry.name.len() + 1)?;
        if let TreeEntryKind::Conflict {
            base, ours, theirs, ..
        } = &entry.kind
        {
            checked_len(&mut length, option_string_len(base.as_deref())?)?;
            checked_len(&mut length, option_string_len(ours.as_deref())?)?;
            checked_len(&mut length, option_string_len(theirs.as_deref())?)?;
            if uses_v2 {
                checked_len(&mut length, 12)?;
            }
        }
        checked_len(&mut length, 8 + entry.hash.len() + 8 + 4)?;
    }
    Ok(length)
}

fn canonical_snapshot_len(snapshot: &Snapshot) -> Result<usize> {
    let mut length = 4;
    checked_len(&mut length, 8 + snapshot.root.len() + 8)?;
    for parent in &snapshot.parents {
        checked_len(&mut length, 8 + parent.len())?;
    }
    checked_len(&mut length, 8 + snapshot.author.len() + 8)?;
    checked_len(&mut length, option_string_len(snapshot.message.as_deref())?)?;
    Ok(length)
}

fn validate_mode(mode: u32) -> Result<()> {
    if mode != 0 && mode != EXECUTABLE_MODE {
        bail!("invalid portable mode {mode}");
    }
    Ok(())
}

fn validate_entry(
    name: &str,
    kind: &TreeEntryKind,
    hash: &str,
    size: u64,
    mode: u32,
) -> Result<()> {
    if !is_safe_rel_path(name) || name.contains('/') {
        bail!("invalid tree entry name {name:?}");
    }
    validate_mode(mode)?;
    if !is_valid_hash(hash) {
        bail!("tree entry contains an invalid object id");
    }
    match kind {
        TreeEntryKind::File => {}
        TreeEntryKind::Dir => {
            if mode != 0 {
                bail!("directory entries cannot be executable");
            }
            if size != 0 {
                bail!("directory entries must have zero size");
            }
        }
        TreeEntryKind::Conflict {
            base,
            ours,
            theirs,
            modes,
        } => {
            for leg in [base, ours, theirs].into_iter().flatten() {
                if !is_valid_hash(leg) {
                    bail!("conflict contains an invalid leg id");
                }
            }
            for leg_mode in [modes.base, modes.ours, modes.theirs] {
                validate_mode(leg_mode)?;
            }
            if base.is_none() && modes.base != 0
                || ours.is_none() && modes.ours != 0
                || theirs.is_none() && modes.theirs != 0
            {
                bail!("absent conflict legs cannot carry executable intent");
            }
            let visible = theirs
                .as_deref()
                .map(|hash| (hash, modes.theirs))
                .or_else(|| ours.as_deref().map(|hash| (hash, modes.ours)))
                .or_else(|| base.as_deref().map(|hash| (hash, modes.base)));
            if visible != Some((hash, mode)) {
                bail!("conflict hash and mode must identify its visible leg");
            }
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

    const fn remaining_len(&self) -> usize {
        self.remaining.len()
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

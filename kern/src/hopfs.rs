//! hopfs: HOP's minimale bestandslaag op de NVMe. De metadata (boom,
//! extents, vrije lijst) leeft in RAM, de data in 4 KB-blokken op de schijf,
//! en sinds 24-09 legt [`commit`] de boom vast (twee plekken, de nieuwste
//! geldige wint) zodat een flip en een koude boot niet leeg beginnen.
//!
//! Eén eigenaar-taak bezit de [`Fs`]: de `FS.mu` uit Go is weg. Het plan
//! staat los van de I/O: [`Fs::read_step`] en [`Fs::write_step`] vertalen
//! één brok synchroon (een schrijf krijgt daar zijn verse run), de I/O doet
//! de aanroeper, en [`Fs::write_landed`] publiceert de mapping pas ná de
//! data. [`read`], [`write`] en [`truncate`] zijn die stappen met de I/O
//! ertussen, op de boom in een [`Tree`] die alleen tussen twee stappen
//! geleend wordt, zodat de actor meerdere calls in de lucht heeft; [`commit`]
//! legt vast terwijl andere calls doorlopen. Eén pad, ook voor de tests en
//! de meetbank.
//!
//! De I/O is een future ([`BlockIo`]): elk blok-verzoek is een submit plus
//! een `.await` op de completion, en tijdens die await draait de executor
//! door. Les van 30-09: de commit (FLUSH, de boom, FLUSH) wachtte tot
//! dan synchroon op het device, en op een trage schijf stond de hele
//! OS-core met Hop en de switch tot 7 s stil. Geen lening van de boom
//! leeft over een await.
//!
//! Paden zijn al door de mount-resolutie heen: dit is de laatste grens, dus
//! `..` is een fout.

use crate::slots::{try_push, try_vec};
use crate::{Error, Result};
use abi::sha256::Sha256;
use alloc::vec::Vec;
use sync::LocalCell;

/// De logische blokmaat (8 NVMe-LBA's van 512 B).
pub const BLOCK_SIZE: usize = 4096;
const BS: u64 = BLOCK_SIZE as u64;
/// Het maximale aantal nodes: de metadata leeft in HOP's heap, en een app die
/// eindeloos kleine bestanden maakt mag HOP niet laten OOM'en (dan vallen
/// alle slots, niet alleen de dader).
pub const MAX_NODES: usize = 1 << 20;
/// Begrenst de fragmentatie-metadata, niet de bestandslengte.
pub const MAX_INDEX_EXTENTS: usize = 1 << 18;
const META_MAGIC: &[u8; 8] = b"HOPFSv01";
const META_VERSION: u32 = 1;
/// 64 MiB per plek: ruim voor ongeveer een miljoen kleine bestanden.
const MAX_SLOT_BLOCKS: u32 = 16384;
const HDR_HASH_OFF: usize = 48;
const HDR_LEN: usize = HDR_HASH_OFF + 32;
/// Tegen een kapotte of vijandige boom die de stack opblaast.
const MAX_DEPTH: usize = 4096;

/// Het blokapparaat onder hopfs. Het contract woont in `blkdev`, onder
/// driver én kern (handboek §7); hier alleen de naam. Er is één vorm: een
/// driver (`blkdev::AsyncBlockDevice`) achter een `blkdev::Queue`; wie vóór
/// de executor mount of meet, draait dezelfde futures af met
/// `blkdev::block_on`.
pub use blkdev::{BatchRead, BlockIo};

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Extent {
    logical: u32,
    physical: u32,
    count: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Run {
    start: u32,
    count: u32,
}

#[derive(Debug)]
struct Node {
    dir: bool,
    /// Gesorteerd op naam.
    children: Vec<(Vec<u8>, usize)>,
    extents: Vec<Extent>,
    size: u64,
}

impl Node {
    fn new(dir: bool) -> Node {
        Node {
            dir,
            children: Vec::new(),
            extents: Vec::new(),
            size: 0,
        }
    }

    fn child(&self, name: &[u8]) -> core::result::Result<usize, usize> {
        self.children
            .binary_search_by(|(n, _)| n.as_slice().cmp(name))
    }

    fn position(&self, block: u32) -> usize {
        self.extents
            .partition_point(|e| u64::from(e.logical) + u64::from(e.count) <= u64::from(block))
    }

    /// Fysiek blok, lengte van de run, en of hij gemapt is.
    fn lookup(&self, block: u32) -> (u32, u32, bool) {
        let i = self.position(block);
        match self.extents.get(i) {
            Some(e) if block >= e.logical => {
                let d = block - e.logical;
                (e.physical + d, e.count - d, true)
            }
            Some(e) => (0, e.logical - block, false),
            None => (0, u32::MAX - block, false),
        }
    }
}

fn adjacent(a: Extent, b: Extent) -> bool {
    u64::from(a.logical) + u64::from(a.count) == u64::from(b.logical)
        && u64::from(a.physical) + u64::from(a.count) == u64::from(b.physical)
}

/// Eén bestandslaag op één venster van één schijf.
pub struct Fs<D> {
    disk: D,
    base: u64,
    lbas_per_block: u64,
    max_io_blocks: u64,
    nodes: Vec<Option<Node>>,
    free_nodes: Vec<usize>,
    free: Vec<Run>,
    next: u32,
    max: u32,
    count: usize,
    index: usize,
    persist: bool,
    slot: u32,
    generation: u64,
    last: Option<u8>,
    dirty: bool,
    pending: Vec<Run>,
}

/// Eén brok van een lees ([`Fs::read_step`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadStep {
    /// Een gat: `len` nullen.
    Hole {
        /// Het aantal bytes.
        len: usize,
    },
    /// Hele blokken: `len` bytes vanaf `lba`, rechtstreeks in de buffer.
    Whole {
        /// De eerste LBA.
        lba: u64,
        /// Het aantal bytes (hele blokken).
        len: usize,
    },
    /// Een deel van één blok: `len` bytes vanaf `at` in het blok op `lba`.
    Part {
        /// De LBA van het blok.
        lba: u64,
        /// De plek in het blok.
        at: usize,
        /// Het aantal bytes.
        len: usize,
    },
}

/// Eén brok van een schrijf ([`Fs::write_step`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteStep {
    /// De LBA van het eerste blok.
    pub lba: u64,
    bi: u32,
    block: u32,
    run: u32,
    mapped: bool,
    /// `None` = hele blokken; anders de plek in het ene blok (lezen,
    /// aanvullen, schrijven).
    pub at: Option<usize>,
    /// Het aantal bytes data van deze brok.
    pub len: usize,
}

/// Een vastlegging in uitvoering ([`Fs::commit_begin`]): de blob van de boom
/// en waar hij heen gaat.
#[derive(Debug)]
pub struct CommitJob {
    blob: Vec<u8>,
    target: u8,
    generation: u64,
    /// De LBA van de kop; de body volgt erachter.
    head: u64,
    per_block: u64,
    step: usize,
    /// De vrijgaven die op deze vastlegging wachtten.
    freed: Vec<Run>,
}

impl CommitJob {
    /// De I/O van de vastlegging: flush (de data van de boom staat er dan),
    /// de body, dan de kop, en weer een flush.
    pub async fn io<B: BlockIo>(&self, disk: &mut B) -> Result {
        disk.flush().await?;
        let (head, body) = self.blob.split_at(BLOCK_SIZE.min(self.blob.len()));
        let blocks = (self.step / BLOCK_SIZE) as u64;
        for (i, chunk) in body.chunks(self.step.max(BLOCK_SIZE)).enumerate() {
            let lba = self.head + (1 + i as u64 * blocks) * self.per_block;
            disk.write(lba, chunk).await?;
        }
        disk.write(self.head, head).await?;
        disk.flush().await?;
        Ok(())
    }
}

/// De I/O van een lees-brok naar `dst` (vooraan); geeft de bytes.
pub async fn read_step_io<B: BlockIo>(
    disk: &mut B,
    s: ReadStep,
    dst: &mut [u8],
    tmp: &mut [u8; BLOCK_SIZE],
) -> Result<usize> {
    match s {
        ReadStep::Hole { len } => {
            dst.get_mut(..len)
                .ok_or(Error::Corrupt { at: len })?
                .fill(0);
            Ok(len)
        }
        ReadStep::Whole { lba, len } => {
            let d = dst.get_mut(..len).ok_or(Error::Corrupt { at: len })?;
            disk.read(lba, d).await?;
            Ok(len)
        }
        ReadStep::Part { lba, at, len } => {
            disk.read(lba, tmp).await?;
            let (d, src) = (dst.get_mut(..len), tmp.get(at..at + len));
            if let (Some(d), Some(src)) = (d, src) {
                d.copy_from_slice(src);
            }
            Ok(len)
        }
    }
}

/// De I/O van een schrijf-brok uit `src`. Een deel van een blok wordt
/// gelezen (als het blok al een huis had), aangevuld en heel geschreven.
pub async fn write_step_io<B: BlockIo>(
    disk: &mut B,
    s: &WriteStep,
    src: &[u8],
    tmp: &mut [u8; BLOCK_SIZE],
) -> Result {
    let Some(at) = s.at else {
        return Ok(disk.write(s.lba, src).await?);
    };
    tmp.fill(0);
    if s.mapped {
        disk.read(s.lba, tmp).await?;
    }
    if let Some(d) = tmp.get_mut(at..at + src.len()) {
        d.copy_from_slice(src);
    }
    Ok(disk.write(s.lba, tmp).await?)
}

/// Wist de staart vanaf `from` van het blok op `lba` (krimpen).
pub async fn clear_tail<B: BlockIo>(
    disk: &mut B,
    lba: u64,
    from: usize,
    tmp: &mut [u8; BLOCK_SIZE],
) -> Result {
    disk.read(lba, tmp).await?;
    if let Some(t) = tmp.get_mut(from..) {
        t.fill(0);
    }
    Ok(disk.write(lba, tmp).await?)
}

const ROOT: usize = 0;

fn split(path: &[u8]) -> Result<Vec<&[u8]>> {
    let mut segs = Vec::new();
    for s in path.split(|b| *b == b'/') {
        match s {
            b"" | b"." => {}
            b".." => return Err(Error::BadPath),
            _ => try_push(&mut segs, s)?,
        }
    }
    Ok(segs)
}

impl<D: BlockIo> Fs<D> {
    /// De lege boom op het venster `[first_lba, +blocks)`, het begin van
    /// [`Fs::mount`]. Blok 0 is `first_lba`: hopfs kan per constructie niet
    /// buiten zijn venster schrijven (de Mac mini: macOS op dezelfde SSD,
    /// 30-08). Zonder mount legt hij niets vast en geeft hij vrij meteen
    /// terug: dat is alleen de vorm van de toetsen (de allocator los van de
    /// schijf); de kern zelf mount altijd.
    fn new(
        disk: D,
        first_lba: u64,
        blocks: u64,
        disk_block: u64,
        max_transfer: u64,
    ) -> Result<Fs<D>> {
        let per = (BS / disk_block.max(1)).max(1);
        let mut nodes = Vec::new();
        try_push(&mut nodes, Some(Node::new(true)))?;
        Ok(Fs {
            disk,
            base: first_lba,
            lbas_per_block: per,
            max_io_blocks: (max_transfer / BS).max(1),
            nodes,
            free_nodes: Vec::new(),
            free: Vec::new(),
            next: 0,
            max: u32::try_from(blocks / per).unwrap_or(u32::MAX),
            count: 0,
            index: 0,
            persist: false,
            slot: 0,
            generation: 0,
            last: None,
            dirty: false,
            pending: Vec::new(),
        })
    }

    fn lba(&self, block: u32) -> u64 {
        self.base + u64::from(block) * self.lbas_per_block
    }

    fn node(&self, i: usize) -> Result<&Node> {
        self.nodes
            .get(i)
            .and_then(Option::as_ref)
            .ok_or(Error::NoEnt)
    }

    fn node_mut(&mut self, i: usize) -> Result<&mut Node> {
        self.nodes
            .get_mut(i)
            .and_then(Option::as_mut)
            .ok_or(Error::NoEnt)
    }

    fn alloc_node(&mut self, dir: bool) -> Result<usize> {
        if self.count >= MAX_NODES {
            return Err(Error::Full { cap: MAX_NODES });
        }
        let i = match self.free_nodes.pop() {
            Some(i) => {
                if let Some(n) = self.nodes.get_mut(i) {
                    *n = Some(Node::new(dir));
                }
                i
            }
            None => {
                try_push(&mut self.nodes, Some(Node::new(dir)))?;
                self.nodes.len() - 1
            }
        };
        self.count += 1;
        self.dirty = true;
        Ok(i)
    }

    fn add_child(&mut self, parent: usize, name: &[u8], dir: bool) -> Result<usize> {
        let pos = match self.node(parent)?.child(name) {
            Ok(p) => {
                return self
                    .node(parent)?
                    .children
                    .get(p)
                    .map(|c| c.1)
                    .ok_or(Error::NoEnt);
            }
            Err(p) => p,
        };
        let name = try_vec(name)?;
        self.node_mut(parent)?
            .children
            .try_reserve(1)
            .map_err(|_| Error::OutOfMemory { bytes: 64 })?;
        let idx = self.alloc_node(dir)?;
        self.node_mut(parent)?.children.insert(pos, (name, idx));
        Ok(idx)
    }

    fn walk(&mut self, segs: &[&[u8]], mk: bool) -> Result<usize> {
        let mut n = ROOT;
        for s in segs {
            let node = self.node(n)?;
            if !node.dir {
                return Err(Error::Kind);
            }
            n = match node.child(s) {
                Ok(p) => node.children.get(p).map(|c| c.1).ok_or(Error::NoEnt)?,
                Err(_) if mk => self.add_child(n, s, true)?,
                Err(_) => return Err(Error::NoEnt),
            };
        }
        Ok(n)
    }

    /// `(size, is_dir)`.
    pub fn stat(&mut self, path: &[u8]) -> Result<(u64, bool)> {
        let n = self.walk(&split(path)?, false)?;
        let node = self.node(n)?;
        Ok((node.size, node.dir))
    }

    /// De namen in een directory, gesorteerd; dirs krijgen een `/`. Meer dan
    /// `max` namen: `Ok(None)`, zonder een gedeeltelijke lijst te bouwen.
    pub fn list_n(&mut self, path: &[u8], max: usize) -> Result<Option<Vec<Vec<u8>>>> {
        let n = self.walk(&split(path)?, false)?;
        let node = self.node(n)?;
        if !node.dir {
            return Err(Error::Kind);
        }
        if node.children.len() > max {
            return Ok(None);
        }
        let mut out = Vec::new();
        for (name, c) in &node.children {
            let mut v = try_vec(name)?;
            if self.node(*c)?.dir {
                try_push(&mut v, b'/')?;
            }
            try_push(&mut out, v)?;
        }
        Ok(Some(out))
    }

    /// De namen in een directory, gesorteerd en `\n`-gescheiden in `dst`
    /// (dirs krijgen een `/`), zonder allocatie: het antwoord van de
    /// system-API gaat zo rechtstreeks in de antwoordbuffer van de
    /// verbinding. Geeft `(aantal, bytes)`; past de lijst niet, dan
    /// [`Error::TooLarge`] zonder half antwoord (`listRespLimit` in Go:
    /// eerst begrenzen, dan pas schrijven).
    pub fn list_into(&mut self, path: &[u8], dst: &mut [u8]) -> Result<(usize, usize)> {
        let n = self.walk(&split(path)?, false)?;
        let node = self.node(n)?;
        if !node.dir {
            return Err(Error::Kind);
        }
        let mut total = 0usize;
        for (i, (name, c)) in node.children.iter().enumerate() {
            let slash = usize::from(self.node(*c)?.dir);
            total += usize::from(i > 0) + name.len() + slash;
            if total > dst.len() {
                return Err(Error::TooLarge {
                    len: total,
                    max: dst.len(),
                });
            }
        }
        let mut at = 0usize;
        for (i, (name, c)) in node.children.iter().enumerate() {
            let mut put = |b: &[u8]| {
                if let Some(d) = dst.get_mut(at..at + b.len()) {
                    d.copy_from_slice(b);
                }
                at += b.len();
            };
            if i > 0 {
                put(b"\n");
            }
            put(name);
            if self.node(*c)?.dir {
                put(b"/");
            }
        }
        Ok((node.children.len(), at))
    }

    /// Maakt een directory, inclusief ouders.
    pub fn mkdir_all(&mut self, path: &[u8]) -> Result {
        let n = self.walk(&split(path)?, true)?;
        if self.node(n)?.dir {
            Ok(())
        } else {
            Err(Error::Kind)
        }
    }

    /// De node van een bestaand pad.
    pub fn find(&mut self, path: &[u8]) -> Result<usize> {
        self.walk(&split(path)?, false)
    }

    /// Hoeveel bytes een lees van hooguit `max` op `off` van node `n`
    /// oplevert; een map is [`Error::Kind`].
    pub fn read_len(&self, n: usize, off: u64, max: usize) -> Result<usize> {
        let node = self.node(n)?;
        if node.dir {
            return Err(Error::Kind);
        }
        if off >= node.size {
            return Ok(0);
        }
        Ok((max as u64).min(node.size - off) as usize)
    }

    /// De volgende brok van een lees van node `n` op `pos`, met nog `left`
    /// bytes te gaan: alleen de vertaling, de I/O doet de aanroeper (zonder
    /// de boom vast te houden, PORT.md §3).
    pub fn read_step(&self, n: usize, pos: u64, left: usize) -> Result<ReadStep> {
        let left = left as u64;
        let (bi, bo) = (pos / BS, pos % BS);
        let (block, run, mapped) = self.node(n)?.lookup(u32::try_from(bi).unwrap_or(u32::MAX));
        let lba = self.lba(block);
        Ok(if !mapped {
            ReadStep::Hole {
                len: (u64::from(run) * BS - bo).min(left) as usize,
            }
        } else if bo == 0 && left >= BS {
            let blocks = u64::from(run).min(self.max_io_blocks).min(left / BS);
            ReadStep::Whole {
                lba,
                len: (blocks * BS) as usize,
            }
        } else {
            ReadStep::Part {
                lba,
                at: bo as usize,
                len: (BS - bo).min(left) as usize,
            }
        })
    }

    fn file(&mut self, path: &[u8]) -> Result<usize> {
        let segs = split(path)?;
        let (name, parents) = segs.split_last().ok_or(Error::BadPath)?;
        let parent = self.walk(parents, true)?;
        let n = self.add_child(parent, name, false)?;
        if self.node(n)?.dir {
            Err(Error::Kind)
        } else {
            Ok(n)
        }
    }

    /// Toetst een schrijf van `len` bytes op `off` en geeft de node van het
    /// bestand (dat zo nodig ontstaat).
    pub fn write_open(&mut self, path: &[u8], off: u64, len: usize) -> Result<usize> {
        let end = off.checked_add(len as u64).ok_or(Error::Range {
            base: off,
            size: len as u64,
        })?;
        if end > u64::from(self.max) * BS {
            return Err(Error::DiskFull {
                blocks: u64::from(self.max),
            });
        }
        self.file(path)
    }

    /// De volgende brok van een schrijf naar node `n` op `pos`, met nog
    /// `left` bytes te gaan. Een blok zonder huis krijgt hier een verse run
    /// (synchroon); de mapping volgt pas in [`Self::write_landed`], ná de
    /// data.
    pub fn write_step(&mut self, n: usize, pos: u64, left: usize) -> Result<WriteStep> {
        let left = left as u64;
        let (bi, bo) = (pos / BS, pos % BS);
        let bi = u32::try_from(bi).map_err(|_| Error::DiskFull { blocks: bi })?;
        let (mut block, avail, mapped) = self.node(n)?.lookup(bi);
        let whole = bo == 0 && left >= BS;
        let mut run = 1u32;
        if whole {
            run =
                u32::try_from(u64::from(avail).min(self.max_io_blocks).min(left / BS)).unwrap_or(1);
        }
        if !mapped {
            (block, run) = self.alloc_run(run)?;
            if self.index >= MAX_INDEX_EXTENTS && !self.joins(n, bi, block, run)? {
                self.free_run(block, run)?;
                return Err(Error::Full {
                    cap: MAX_INDEX_EXTENTS,
                });
            }
        }
        let (at, len) = if whole {
            (None, (u64::from(run) * BS) as usize)
        } else {
            (Some(bo as usize), (BS - bo).min(left) as usize)
        };
        Ok(WriteStep {
            lba: self.lba(block),
            bi,
            block,
            run,
            mapped,
            at,
            len,
        })
    }

    /// De brok van [`Self::write_step`] is terug van het device met `res`:
    /// gelukt, dan de mapping erbij (pas nu) en de maat; mislukt, dan de
    /// verse run terug en de fout. Geeft de geschreven bytes.
    pub fn write_landed(
        &mut self,
        n: usize,
        s: &WriteStep,
        pos: u64,
        res: Result,
    ) -> Result<usize> {
        if let Err(e) = res {
            if !s.mapped {
                self.free_run(s.block, s.run)?;
            }
            return Err(e);
        }
        if !s.mapped {
            self.map_run(
                n,
                Extent {
                    logical: s.bi,
                    physical: s.block,
                    count: s.run,
                },
            )?;
        }
        let end = pos + s.len as u64;
        let node = self.node_mut(n)?;
        if end > node.size {
            node.size = end;
        }
        self.dirty = true;
        Ok(s.len)
    }

    /// Toetst een truncate en geeft de node van het bestand (dat zo nodig
    /// ontstaat).
    pub fn truncate_open(&mut self, path: &[u8], size: u64) -> Result<usize> {
        if size > u64::from(self.max) * BS {
            return Err(Error::DiskFull {
                blocks: u64::from(self.max),
            });
        }
        self.file(path)
    }

    /// Het blok waarvan de staart vanaf `size` gewist moet worden vóór het
    /// krimpen, als dat er is.
    pub fn truncate_tail(&self, n: usize, size: u64) -> Result<Option<u64>> {
        if size >= self.node(n)?.size || size.is_multiple_of(BS) {
            return Ok(None);
        }
        let (block, _, mapped) = self.node(n)?.lookup(u32::try_from(size / BS).unwrap_or(0));
        Ok(mapped.then(|| self.lba(block)))
    }

    /// De maat van node `n` wordt `size`: bij krimpen gaan de runs erachter
    /// terug (vastgelegd pas na de volgende commit).
    pub fn truncate_set(&mut self, n: usize, size: u64) -> Result {
        if size < self.node(n)?.size {
            let need = u32::try_from(size.div_ceil(BS)).unwrap_or(u32::MAX);
            let extents = core::mem::take(&mut self.node_mut(n)?.extents);
            let mut keep = Vec::new();
            for mut e in extents {
                if e.logical >= need {
                    self.drop_run(e.physical, e.count)?;
                    self.index -= 1;
                    continue;
                }
                if u64::from(e.logical) + u64::from(e.count) > u64::from(need) {
                    let retain = need - e.logical;
                    self.drop_run(e.physical + retain, e.count - retain)?;
                    e.count = retain;
                }
                try_push(&mut keep, e)?;
            }
            self.node_mut(n)?.extents = keep;
        }
        self.node_mut(n)?.size = size;
        self.dirty = true;
        Ok(())
    }

    /// Verwijdert een bestand of lege directory; `recursive` een hele boom.
    pub fn remove(&mut self, path: &[u8], recursive: bool) -> Result {
        let segs = split(path)?;
        let (name, parents) = segs.split_last().ok_or(Error::BadPath)?;
        let parent = self.walk(parents, false)?;
        let pos = self.node(parent)?.child(name).map_err(|_| Error::NoEnt)?;
        let idx = self
            .node(parent)?
            .children
            .get(pos)
            .map(|c| c.1)
            .ok_or(Error::NoEnt)?;
        if self.node(idx)?.dir && !self.node(idx)?.children.is_empty() && !recursive {
            return Err(Error::NotEmpty);
        }
        self.node_mut(parent)?.children.remove(pos);
        let mut stack = Vec::new();
        try_push(&mut stack, idx)?;
        while let Some(i) = stack.pop() {
            let Some(node) = self.nodes.get_mut(i).and_then(Option::take) else {
                continue;
            };
            self.count -= 1;
            let _ = try_push(&mut self.free_nodes, i);
            for (_, c) in node.children {
                try_push(&mut stack, c)?;
            }
            self.index -= node.extents.len();
            for e in node.extents {
                self.drop_run(e.physical, e.count)?;
            }
        }
        self.dirty = true;
        Ok(())
    }

    fn joins(&self, n: usize, logical: u32, physical: u32, count: u32) -> Result<bool> {
        let node = self.node(n)?;
        let i = node.position(logical);
        let e = Extent {
            logical,
            physical,
            count,
        };
        let before = i > 0 && node.extents.get(i - 1).is_some_and(|p| adjacent(*p, e));
        let after = node.extents.get(i).is_some_and(|x| adjacent(e, *x));
        Ok(before || after)
    }

    /// Voegt een nog ongemapte run in en smelt met beide buren.
    fn map_run(&mut self, n: usize, e: Extent) -> Result {
        let node = self
            .nodes
            .get_mut(n)
            .and_then(Option::as_mut)
            .ok_or(Error::NoEnt)?;
        let mut i = node.position(e.logical);
        if i > 0 && node.extents.get(i - 1).is_some_and(|p| adjacent(*p, e)) {
            i -= 1;
            if let Some(p) = node.extents.get_mut(i) {
                p.count += e.count;
            }
        } else {
            node.extents
                .try_reserve(1)
                .map_err(|_| Error::OutOfMemory { bytes: 12 })?;
            node.extents.insert(i, e);
            self.index += 1;
        }
        if let (Some(a), Some(b)) = (
            node.extents.get(i).copied(),
            node.extents.get(i + 1).copied(),
        ) && adjacent(a, b)
        {
            if let Some(a) = node.extents.get_mut(i) {
                a.count += b.count;
            }
            node.extents.remove(i + 1);
            self.index -= 1;
        }
        Ok(())
    }

    /// Bump eerst, dan de vrije lijst; runs zo groot als een transfer.
    fn alloc_run(&mut self, want: u32) -> Result<(u32, u32)> {
        if self.next < self.max {
            let count = want.min(self.max - self.next);
            let start = self.next;
            self.next += count;
            return Ok((start, count));
        }
        if let Some(r) = self.free.first_mut() {
            let (start, count) = (r.start, want.min(r.count));
            r.start += count;
            r.count -= count;
            if r.count == 0 {
                self.free.remove(0);
            }
            return Ok((start, count));
        }
        Err(Error::DiskFull {
            blocks: u64::from(self.max),
        })
    }

    /// Gesorteerd en samengevoegd; een staart tegen de bump-pointer gaat
    /// terug naar de bump (ook de exacte rollback).
    fn free_run(&mut self, start: u32, count: u32) -> Result {
        if count == 0 {
            return Ok(());
        }
        let mut i = self.free.partition_point(|r| r.start < start);
        let end_of = |r: &Run| u64::from(r.start) + u64::from(r.count);
        if i > 0
            && self
                .free
                .get(i - 1)
                .is_some_and(|p| end_of(p) == u64::from(start))
        {
            i -= 1;
            if let Some(p) = self.free.get_mut(i) {
                p.count += count;
            }
        } else {
            self.free
                .try_reserve(1)
                .map_err(|_| Error::OutOfMemory { bytes: 8 })?;
            self.free.insert(i, Run { start, count });
        }
        if let (Some(a), Some(b)) = (self.free.get(i).copied(), self.free.get(i + 1).copied())
            && end_of(&a) == u64::from(b.start)
        {
            if let Some(a) = self.free.get_mut(i) {
                a.count += b.count;
            }
            self.free.remove(i + 1);
        }
        if let Some(last) = self.free.last().copied()
            && end_of(&last) == u64::from(self.next)
        {
            self.next = last.start;
            self.free.pop();
        }
        Ok(())
    }

    /// Vrijgeven: vluchtig meteen, vastgelegd pas na de volgende commit.
    /// Anders wijst de boom op schijf na een stroomuitval naar andermans data.
    fn drop_run(&mut self, start: u32, count: u32) -> Result {
        if !self.persist {
            return self.free_run(start, count);
        }
        if count > 0 {
            try_push(&mut self.pending, Run { start, count })?;
        }
        Ok(())
    }
}

/// De boom van de actor met meerdere calls in de lucht: elke call leent
/// hem alleen binnen één stap (synchroon), en doet zijn I/O daartussen met
/// een eigen kopie van het gedeelde handvat (de wachtrij). Zo loopt de
/// metadata-lening nooit over een await (PORT.md §3), en wachten twee
/// calls nooit op elkaars schijf.
pub type Tree<D> = LocalCell<Fs<D>>;

/// Leest hooguit `buf.len()` bytes van node `n` (uit [`Fs::find`]) op
/// `off`; gaten lezen als nul. Geeft de gelezen bytes.
pub async fn read<D: BlockIo + Copy>(
    t: &Tree<D>,
    n: usize,
    off: u64,
    buf: &mut [u8],
) -> Result<usize> {
    let (want, mut disk) = {
        let fs = t.borrow();
        (fs.read_len(n, off, buf.len())?, fs.disk())
    };
    let mut tmp = [0u8; BLOCK_SIZE];
    let mut done = 0usize;
    while done < want {
        let step = t.borrow().read_step(n, off + done as u64, want - done)?;
        let dst = buf.get_mut(done..want).unwrap_or(&mut []);
        done += read_step_io(&mut disk, step, dst, &mut tmp).await?;
    }
    Ok(done)
}

/// Schrijft `data` op `off` in node `n` (uit [`Fs::write_open`]). Eerdere
/// geslaagde brokken blijven bij een I/O-fout; een verse mapping wordt pas
/// NA de data gepubliceerd ([`Fs::write_landed`]), zodat een oude eigenaar
/// nooit zichtbaar wordt. Eén schrijver per bestand tegelijk: dat bewaakt
/// de aanroeper (twee verse runs voor hetzelfde blok zouden allebei gemapt
/// willen worden).
pub async fn write<D: BlockIo + Copy>(t: &Tree<D>, n: usize, off: u64, data: &[u8]) -> Result {
    let mut disk = t.borrow().disk();
    let mut tmp = [0u8; BLOCK_SIZE];
    let mut done = 0usize;
    while done < data.len() {
        let pos = off + done as u64;
        let step = t.borrow_mut().write_step(n, pos, data.len() - done)?;
        let src = data
            .get(done..done + step.len)
            .ok_or(Error::Corrupt { at: done })?;
        let res = write_step_io(&mut disk, &step, src, &mut tmp).await;
        done += t.borrow_mut().write_landed(n, &step, pos, res)?;
    }
    Ok(())
}

/// Zet de maat van node `n` (uit [`Fs::truncate_open`]) op `size`. Groeit
/// ijl; krimpen geeft runs terug en wist de bewaarde staart, zodat een
/// latere groei geen weggegooide bytes laat zien. Krimpen geeft blokken
/// vrij: de aanroeper laat dit alleen toe als er niets anders loopt.
pub async fn truncate<D: BlockIo + Copy>(t: &Tree<D>, n: usize, size: u64) -> Result {
    let (tail, mut disk) = {
        let fs = t.borrow();
        (fs.truncate_tail(n, size)?, fs.disk())
    };
    if let Some(lba) = tail {
        let mut tmp = [0u8; BLOCK_SIZE];
        clear_tail(&mut disk, lba, (size % BS) as usize, &mut tmp).await?;
    }
    t.borrow_mut().truncate_set(n, size)
}

/// Legt de boom van nu vast als hij veranderde, terwijl andere calls
/// doorlopen: data flushen, boom in de ANDERE plek (eerst de body, dan de
/// kop), weer flushen, en pas dan de uitgestelde vrijgaven echt vrijgeven
/// ([`Fs::commit_begin`], [`CommitJob::io`], [`Fs::commit_end`]). De kop
/// komt als laatste, dus een gescheurde schrijf maakt alleen de nieuwe plek
/// ongeldig. Geeft de nieuwe generatie, of `None` als er niets te doen was.
/// Hoogstens één tegelijk: dat bewaakt de aanroeper. Een fout laat de boom
/// in het geheugen achter zonder de vastlegging die hij beloofde: de
/// aanroeper valt dan (`fatal`, `crate::rpc`), er is geen tweede poging.
///
/// De flip bevriest hopfs door dit te roepen en daarna geen verzoek meer
/// aan te nemen: de eigenaar-taak IS het slot dat `Freeze` in Go was.
pub async fn commit<D: BlockIo + Copy>(t: &Tree<D>) -> Result<Option<u64>> {
    let (job, mut disk) = {
        let mut fs = t.borrow_mut();
        (fs.commit_begin()?, fs.disk())
    };
    let Some(job) = job else {
        return Ok(None);
    };
    job.io(&mut disk).await?;
    let mut fs = t.borrow_mut();
    fs.commit_end(job)?;
    Ok(Some(fs.generation()))
}

/// De expliciete duurzame barrière: een vastlegging, of zonder verandering
/// alleen een device-flush. Geeft de bevestigde generatie.
pub async fn sync<D: BlockIo + Copy>(t: &Tree<D>) -> Result<u64> {
    if commit(t).await?.is_none() {
        let mut disk = t.borrow().disk();
        disk.flush().await?;
    }
    Ok(t.borrow().generation())
}

/// De calls op een pad, voor wie zonder actor leest en schrijft: de tests en
/// de meetbank. Dezelfde [`read`], [`write`] en [`truncate`], met de node
/// eerst opgezocht.
pub mod path {
    use super::{BlockIo, Result, Tree};

    /// Leest hooguit `buf.len()` bytes van `path` op `off`.
    pub async fn read<D: BlockIo + Copy>(
        t: &Tree<D>,
        path: &[u8],
        off: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        let n = t.borrow_mut().find(path)?;
        super::read(t, n, off, buf).await
    }

    /// Schrijft `data` op `off` in `path`; het bestand ontstaat zo nodig.
    pub async fn write<D: BlockIo + Copy>(
        t: &Tree<D>,
        path: &[u8],
        off: u64,
        data: &[u8],
    ) -> Result {
        let n = t.borrow_mut().write_open(path, off, data.len())?;
        super::write(t, n, off, data).await
    }

    /// Zet de maat van `path` op `size`; het bestand ontstaat zo nodig.
    pub async fn truncate<D: BlockIo + Copy>(t: &Tree<D>, path: &[u8], size: u64) -> Result {
        let n = t.borrow_mut().truncate_open(path, size)?;
        super::truncate(t, n, size).await
    }
}

/// Wat [`Fs::mount`] aantrof, voor de ene consoleregel.
#[derive(Debug, PartialEq, Eq)]
pub enum Mounted {
    /// `fresh`: beide plekken gewist.
    Fresh,
    /// Geen geldige boom: leeg begonnen.
    Empty,
    /// De boom van deze generatie uit deze plek.
    Restored {
        /// De generatie.
        generation: u64,
        /// De plek (0 of 1).
        slot: u8,
    },
}

impl<D: BlockIo> Fs<D> {
    /// Een bestandslaag die zijn boom vastlegt. Het metagebied is twee
    /// plekken van `min(16384, blocks/64)` blokken aan het begin.
    ///
    /// Hard (docs/sequence-hopos.md §13.6): een venster dat te klein is voor
    /// twee plekken, een blokfout, en een geldige boom die niet ontcijfert,
    /// zijn een fout; de binary valt erop. Alleen "geen enkele geldige plek"
    /// is een lege boom. Tot 01-11 werd dat respectievelijk een vluchtige
    /// fs, een "ongeldige plek" en een lege boom die de oudere plek bij de
    /// eerste commit overschreef (F12).
    pub async fn mount(
        disk: D,
        first_lba: u64,
        blocks: u64,
        disk_block: u64,
        max_transfer: u64,
        fresh: bool,
    ) -> Result<(Fs<D>, Mounted)> {
        let mut f = Fs::new(disk, first_lba, blocks, disk_block, max_transfer)?;
        let slot = MAX_SLOT_BLOCKS.min(f.max / 64);
        if slot < 2 {
            return Err(Error::DiskFull {
                blocks: u64::from(f.max),
            });
        }
        (f.persist, f.slot, f.last, f.next) = (true, slot, None, 2 * slot);
        if fresh {
            let zero = [0u8; BLOCK_SIZE];
            for p in 0..2 {
                f.disk.write(f.lba(p * slot), &zero).await?;
            }
            return Ok((f, Mounted::Fresh));
        }
        let mut best: Option<(u8, u64, Vec<u8>)> = None;
        for p in 0..2u8 {
            if let Some((generation, tree)) = f.read_slot(p).await?
                && best.as_ref().is_none_or(|b| generation > b.1)
            {
                best = Some((p, generation, tree));
            }
        }
        let Some((p, generation, tree)) = best else {
            return Ok((f, Mounted::Empty));
        };
        f.generation = generation;
        f.last = Some(p);
        f.decode(&tree)?;
        Ok((
            f,
            Mounted::Restored {
                generation,
                slot: p,
            },
        ))
    }

    /// Geeft het blokapparaat terug (een test die opnieuw mount).
    #[cfg(test)]
    pub(crate) fn into_disk(self) -> D {
        self.disk
    }

    /// De generatie van de laatst weggeschreven boom.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Het begin van een vastlegging: de boom van NU, als blob voor de
    /// andere plek (`None` als er niets vast te leggen is). Synchroon, zodat
    /// de actor intussen verder kan: wat na dit punt verandert, zet `dirty`
    /// weer en gaat met de volgende mee, en wat na dit punt vrijkomt, wacht
    /// op die volgende (de boom van deze vastlegging wijst er nog naar).
    /// Hoogstens één vastlegging tegelijk: de aanroeper zorgt daarvoor.
    pub fn commit_begin(&mut self) -> Result<Option<CommitJob>> {
        if !self.persist || (!self.dirty && self.last.is_some()) {
            return Ok(None);
        }
        let tree = self.encode()?;
        let room = (u64::from(self.slot) - 1) * BS;
        if tree.len() as u64 > room {
            return Err(Error::TooLarge {
                len: tree.len(),
                max: room as usize,
            });
        }
        let target = if self.last == Some(0) { 1u8 } else { 0 };
        let generation = self.generation + 1;
        let blob = self.blob(generation, &tree)?;
        let start = u32::from(target) * self.slot;
        let job = CommitJob {
            blob,
            target,
            generation,
            head: self.lba(start),
            per_block: self.lbas_per_block,
            step: self.max_io_blocks as usize * BLOCK_SIZE,
            freed: core::mem::take(&mut self.pending),
        };
        self.dirty = false;
        Ok(Some(job))
    }

    /// Het einde van een geslaagde [`CommitJob::io`]: dit is de generatie,
    /// en wat de vorige boom nog vasthield, komt vrij.
    pub fn commit_end(&mut self, job: CommitJob) -> Result {
        let CommitJob {
            target,
            generation,
            freed,
            ..
        } = job;
        (self.generation, self.last) = (generation, Some(target));
        for run in freed {
            self.free_run(run.start, run.count)?;
        }
        Ok(())
    }

    /// Het blokapparaat zelf, als het een gedeeld handvat is (de wachtrij):
    /// voor wie I/O doet zonder de boom vast te houden.
    pub fn disk(&self) -> D
    where
        D: Copy,
    {
        self.disk
    }

    fn header(&self, generation: u64, tree_len: u64) -> [u8; HDR_HASH_OFF] {
        let mut h = [0u8; HDR_HASH_OFF];
        let mut put = |off: usize, b: &[u8]| {
            if let Some(d) = h.get_mut(off..off + b.len()) {
                d.copy_from_slice(b);
            }
        };
        put(0, META_MAGIC);
        put(8, &META_VERSION.to_le_bytes());
        put(12, &self.slot.to_le_bytes());
        put(16, &generation.to_le_bytes());
        put(24, &self.base.to_le_bytes());
        put(32, &self.max.to_le_bytes());
        put(36, &(self.lbas_per_block as u32).to_le_bytes());
        put(40, &tree_len.to_le_bytes());
        h
    }

    fn blob(&self, generation: u64, tree: &[u8]) -> Result<Vec<u8>> {
        let n = BLOCK_SIZE + tree.len().div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
        let mut b = Vec::new();
        b.try_reserve_exact(n)
            .map_err(|_| Error::OutOfMemory { bytes: n })?;
        b.resize(n, 0);
        let hdr = self.header(generation, tree.len() as u64);
        let mut sum = Sha256::new();
        sum.update(&hdr);
        sum.update(tree);
        if let Some(d) = b.get_mut(..HDR_HASH_OFF) {
            d.copy_from_slice(&hdr);
        }
        if let Some(d) = b.get_mut(HDR_HASH_OFF..HDR_LEN) {
            d.copy_from_slice(&sum.finish());
        }
        if let Some(d) = b.get_mut(BLOCK_SIZE..BLOCK_SIZE + tree.len()) {
            d.copy_from_slice(tree);
        }
        Ok(b)
    }

    /// De boom in plek `p`, of `None` als daar geen geldige staat (leeg, een
    /// ander venster, een gescheurde schrijf). Een blokfout is een fout.
    async fn read_slot(&mut self, p: u8) -> Result<Option<(u64, Vec<u8>)>> {
        let mut hdr = [0u8; BLOCK_SIZE];
        let start = u32::from(p) * self.slot;
        self.disk.read(self.lba(start), &mut hdr).await?;
        let u64_at = |o: usize| {
            hdr.get(o..o + 8)
                .and_then(|s| <[u8; 8]>::try_from(s).ok())
                .map_or(0, u64::from_le_bytes)
        };
        let generation = u64_at(16);
        let size = u64_at(40);
        if hdr.get(..HDR_HASH_OFF) != Some(&self.header(generation, size)[..])
            || size > (u64::from(self.slot) - 1) * BS
        {
            return Ok(None); // Leeg of een ander venster.
        }
        let padded = (size as usize).div_ceil(BLOCK_SIZE) * BLOCK_SIZE;
        let mut tree = Vec::new();
        tree.try_reserve_exact(padded)
            .map_err(|_| Error::OutOfMemory { bytes: padded })?;
        tree.resize(padded, 0);
        let step = self.max_io_blocks as usize * BLOCK_SIZE;
        for (i, chunk) in tree.chunks_mut(step).enumerate() {
            let lba = self.lba(start + 1 + (i * step / BLOCK_SIZE) as u32);
            self.disk.read(lba, chunk).await?;
        }
        tree.truncate(size as usize);
        let mut sum = Sha256::new();
        sum.update(hdr.get(..HDR_HASH_OFF).unwrap_or(&[]));
        sum.update(&tree);
        if hdr.get(HDR_HASH_OFF..HDR_LEN) != Some(&sum.finish()[..]) {
            return Ok(None); // Gescheurde schrijf.
        }
        Ok(Some((generation, tree)))
    }

    fn encode(&self) -> Result<Vec<u8>> {
        let mut b = Vec::new();
        self.put_header(&mut b, ROOT)?;
        // Iteratief pre-order: een diepe boom mag de stack niet opblazen.
        let mut stack: Vec<(usize, usize)> = Vec::new();
        try_push(&mut stack, (ROOT, 0))?;
        while let Some(top) = stack.last_mut() {
            let (i, k) = *top;
            let Some((name, c)) = self.node(i)?.children.get(k) else {
                stack.pop();
                continue;
            };
            top.1 += 1;
            ext(&mut b, &(name.len() as u16).to_le_bytes())?;
            ext(&mut b, name)?;
            self.put_header(&mut b, *c)?;
            if self.node(*c)?.dir {
                try_push(&mut stack, (*c, 0))?;
            }
        }
        Ok(b)
    }

    fn put_header(&self, b: &mut Vec<u8>, i: usize) -> Result {
        let n = self.node(i)?;
        if n.dir {
            ext(b, &[1])?;
            return ext(b, &(n.children.len() as u32).to_le_bytes());
        }
        ext(b, &[0])?;
        ext(b, &n.size.to_le_bytes())?;
        ext(b, &(n.extents.len() as u32).to_le_bytes())?;
        for e in &n.extents {
            ext(b, &e.logical.to_le_bytes())?;
            ext(b, &e.physical.to_le_bytes())?;
            ext(b, &e.count.to_le_bytes())?;
        }
        Ok(())
    }

    /// Leest een vastgelegde boom terug. Elke afwijking is een fout, en de
    /// staat verandert pas als alles klopt: dubbel eigendom van een blok,
    /// een extent buiten het datagebied, een naam met `/`.
    fn decode(&mut self, b: &[u8]) -> Result {
        let mut r = Reader { b, pos: 0 };
        let meta = 2 * self.slot;
        let mut nodes: Vec<Option<Node>> = Vec::new();
        let (mut count, mut index) = (0usize, 0usize);
        let mut runs: Vec<Run> = Vec::new();
        let root = self.read_node(&mut r, meta, &mut index, &mut runs)?;
        if !root.dir {
            return Err(Error::Kind);
        }
        let root_n = root.size as usize; // Bij een dir: het aantal kinderen.
        try_push(&mut nodes, Some(Node { size: 0, ..root }))?;
        let mut stack: Vec<(usize, usize)> = Vec::new();
        try_push(&mut stack, (ROOT, root_n))?;
        while let Some(top) = stack.last_mut() {
            if top.1 == 0 {
                stack.pop();
                continue;
            }
            top.1 -= 1;
            let parent = top.0;
            let len = usize::from(u16::from_le_bytes(r.array()?));
            let name = r.take(len)?;
            if name.is_empty() || name == b"." || name == b".." || name.contains(&b'/') {
                return Err(Error::BadPath);
            }
            count += 1;
            if count > MAX_NODES || stack.len() > MAX_DEPTH {
                return Err(Error::Full { cap: MAX_NODES });
            }
            let child = self.read_node(&mut r, meta, &mut index, &mut runs)?;
            let kids = child.size as usize;
            let dir = child.dir;
            let child = if dir {
                Node { size: 0, ..child }
            } else {
                child
            };
            try_push(&mut nodes, Some(child))?;
            let idx = nodes.len() - 1;
            let p = nodes
                .get_mut(parent)
                .and_then(Option::as_mut)
                .ok_or(Error::NoEnt)?;
            let pos = match p.child(name) {
                Ok(_) => return Err(Error::Corrupt { at: r.pos }), // Dubbele naam.
                Err(pos) => pos,
            };
            p.children
                .try_reserve(1)
                .map_err(|_| Error::OutOfMemory { bytes: 64 })?;
            p.children.insert(pos, (try_vec(name)?, idx));
            if dir {
                try_push(&mut stack, (idx, kids))?;
            }
        }
        if r.pos != b.len() {
            return Err(Error::Corrupt { at: r.pos });
        }
        runs.sort_unstable_by_key(|r| r.start);
        let mut free = Vec::new();
        let mut next = meta;
        for run in &runs {
            if run.start < next {
                return Err(Error::Corrupt {
                    at: run.start as usize,
                }); // Twee eigenaren.
            }
            if run.start > next {
                try_push(
                    &mut free,
                    Run {
                        start: next,
                        count: run.start - next,
                    },
                )?;
            }
            next = run.start + run.count;
        }
        self.nodes = nodes;
        self.free_nodes = Vec::new();
        (self.count, self.index, self.free, self.next) = (count, index, free, next);
        self.dirty = false;
        self.pending = Vec::new();
        Ok(())
    }

    /// Leest één node-kop. Bij een directory staat het aantal kinderen
    /// tijdelijk in `size`.
    fn read_node(
        &self,
        r: &mut Reader<'_>,
        meta: u32,
        index: &mut usize,
        runs: &mut Vec<Run>,
    ) -> Result<Node> {
        match r.take(1)? {
            [1] => {
                let mut n = Node::new(true);
                n.size = u64::from(u32::from_le_bytes(r.array()?));
                Ok(n)
            }
            [0] => {
                let mut n = Node::new(false);
                n.size = u64::from_le_bytes(r.array()?);
                let cnt = u32::from_le_bytes(r.array()?) as usize;
                *index += cnt;
                if *index > MAX_INDEX_EXTENTS || n.size > u64::from(self.max) * BS {
                    return Err(Error::Corrupt { at: r.pos });
                }
                let need = n.size.div_ceil(BS);
                let mut prev_end = 0u64;
                for k in 0..cnt {
                    let e = Extent {
                        logical: u32::from_le_bytes(r.array()?),
                        physical: u32::from_le_bytes(r.array()?),
                        count: u32::from_le_bytes(r.array()?),
                    };
                    let l_end = u64::from(e.logical) + u64::from(e.count);
                    let p_end = u64::from(e.physical) + u64::from(e.count);
                    if e.count == 0
                        || (k > 0 && u64::from(e.logical) < prev_end)
                        || l_end > need
                        || e.physical < meta
                        || p_end > u64::from(self.max)
                    {
                        return Err(Error::Corrupt { at: r.pos });
                    }
                    prev_end = l_end;
                    try_push(&mut n.extents, e)?;
                    try_push(
                        runs,
                        Run {
                            start: e.physical,
                            count: e.count,
                        },
                    )?;
                }
                Ok(n)
            }
            _ => Err(Error::Corrupt { at: r.pos }),
        }
    }
}

fn ext(b: &mut Vec<u8>, s: &[u8]) -> Result {
    b.try_reserve(s.len())
        .map_err(|_| Error::OutOfMemory { bytes: s.len() })?;
    b.extend_from_slice(s);
    Ok(())
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let s = self
            .b
            .get(self.pos..self.pos.saturating_add(n))
            .ok_or(Error::Corrupt { at: self.pos })?;
        self.pos += n;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let at = self.pos;
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| Error::Corrupt { at })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::{Cell, RefCell};
    use std::vec;

    /// Een schijf in RAM die telt wat er langskomt. Hopfs leent hem als
    /// gedeeld handvat (`&RamDisk`), zoals de kern de wachtrij.
    struct RamDisk {
        block: u64,
        data: RefCell<Vec<u8>>,
        writes: RefCell<Vec<(u64, usize)>>,
        fail: Cell<bool>,
    }

    impl RamDisk {
        fn new(block: u64, bytes: usize) -> RamDisk {
            RamDisk {
                block,
                data: RefCell::new(vec![0; bytes]),
                writes: RefCell::new(Vec::new()),
                fail: Cell::new(false),
            }
        }

        fn blocks(&self) -> u64 {
            self.data.borrow().len() as u64 / self.block
        }
    }

    /// Een future van hopfs afdraaien: de RAM-schijf is meteen klaar.
    fn on<F: core::future::Future>(f: F) -> F::Output {
        blkdev::block_on(f)
    }

    impl BlockIo for &RamDisk {
        async fn read(&mut self, lba: u64, buf: &mut [u8]) -> blkdev::Result {
            let off = (lba * self.block) as usize;
            let data = self.data.borrow();
            let s = data
                .get(off..off + buf.len())
                .ok_or(blkdev::Error::Io { lba })?;
            buf.copy_from_slice(s);
            Ok(())
        }
        async fn write(&mut self, lba: u64, buf: &[u8]) -> blkdev::Result {
            if self.fail.get() {
                return Err(blkdev::Error::Io { lba });
            }
            let off = (lba * self.block) as usize;
            let mut data = self.data.borrow_mut();
            let d = data
                .get_mut(off..off + buf.len())
                .ok_or(blkdev::Error::Io { lba })?;
            d.copy_from_slice(buf);
            self.writes.borrow_mut().push((lba, buf.len()));
            Ok(())
        }
        async fn flush(&mut self) -> blkdev::Result {
            Ok(())
        }
    }

    fn pattern(n: usize, seed: u8) -> Vec<u8> {
        (0..n)
            .map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed))
            .collect()
    }

    type Vol<'d> = Tree<&'d RamDisk>;

    fn vol(d: &RamDisk) -> Vol<'_> {
        LocalCell::cell(Fs::new(d, 0, d.blocks(), d.block, 1 << 20).unwrap())
    }

    fn write(f: &Vol<'_>, p: &[u8], off: u64, data: &[u8]) -> Result {
        on(path::write(f, p, off, data))
    }

    fn must_read(f: &Vol<'_>, p: &[u8], want: &[u8]) {
        let mut got = vec![0u8; want.len()];
        assert_eq!(on(path::read(f, p, 0, &mut got)).unwrap(), want.len());
        assert!(got == want, "content differs");
    }

    #[test]
    fn aaneengesloten_io_wordt_per_mi_b_gebundeld() {
        let d = RamDisk::new(512, 4 << 20);
        let f = vol(&d);
        let want = pattern(2 << 20, 7);
        write(&f, b"/data.bin", 0, &want).unwrap();
        must_read(&f, b"/data.bin", &want);
        drop(f);
        assert_eq!(*d.writes.borrow(), vec![(0, 1 << 20), (2048, 1 << 20)]);
    }

    #[test]
    fn list_n_sorteert_binnen_limiet() {
        let d = RamDisk::new(4096, 1 << 20);
        let f = vol(&d);
        for n in [&b"b"[..], b"a", b"c"] {
            write(&f, &[b"/d/", n].concat(), 0, b"x").unwrap();
        }
        f.borrow_mut().mkdir_all(b"/d/sub").unwrap();
        let names = f.borrow_mut().list_n(b"/d", 4).unwrap().unwrap();
        assert_eq!(
            names,
            vec![
                b"a".to_vec(),
                b"b".to_vec(),
                b"c".to_vec(),
                b"sub/".to_vec()
            ]
        );
        assert_eq!(
            f.borrow_mut().list_n(b"/d", 3).unwrap(),
            None,
            "partial listing built"
        );
    }

    #[test]
    fn paths_reject_dot_dot_and_kinds() {
        let d = RamDisk::new(4096, 1 << 20);
        let f = vol(&d);
        assert_eq!(write(&f, b"/a/../b", 0, b"x"), Err(Error::BadPath));
        write(&f, b"/f", 0, b"x").unwrap();
        assert_eq!(f.borrow_mut().mkdir_all(b"/f"), Err(Error::Kind));
        assert_eq!(f.borrow_mut().stat(b"/nope"), Err(Error::NoEnt));
        f.borrow_mut().mkdir_all(b"/d/e").unwrap();
        assert_eq!(f.borrow_mut().remove(b"/d", false), Err(Error::NotEmpty));
        f.borrow_mut().remove(b"/d", true).unwrap();
        assert_eq!(f.borrow_mut().stat(b"/d/e"), Err(Error::NoEnt));
    }

    // Een venster: blok 0 ligt op de eerste LBA van het venster.
    #[test]
    fn venster_verschuift_en_begrenst() {
        let d = RamDisk::new(4096, 64 << 12);
        let f = LocalCell::cell(Fs::new(&d, 16, 8, 4096, 1 << 20).unwrap());
        write(&f, b"x", 0, &[1u8; 4096]).unwrap();
        let big = vec![1u8; 8 * 4096];
        assert!(write(&f, b"y", 0, &big).is_err(), "wrote beyond window");
        drop(f);
        assert_eq!(d.writes.borrow().first(), Some(&(16, 4096)));
    }

    #[test]
    fn leeg_venster_deelt_niets_uit() {
        let d = RamDisk::new(4096, 1 << 16);
        let f = LocalCell::cell(Fs::new(&d, 4096, 0, 4096, 1 << 20).unwrap());
        assert!(write(&f, b"x", 0, b"a").is_err());
    }

    #[test]
    fn truncate_and_recycled_write_failure_do_not_expose_old_data() {
        let d = RamDisk::new(4096, 1 << 20);
        let f = vol(&d);
        write(&f, b"f", 0, &[0xAA; 8192]).unwrap();
        on(path::truncate(&f, b"f", 100)).unwrap();
        on(path::truncate(&f, b"f", 8192)).unwrap();
        let mut got = [0u8; 8192];
        on(path::read(&f, b"f", 0, &mut got)).unwrap();
        assert!(
            got[100..].iter().all(|b| *b == 0),
            "discarded bytes reappeared"
        );
        d.fail.set(true);
        assert!(write(&f, b"g", 0, &[1; 4096]).is_err());
        d.fail.set(false);
        assert_eq!(
            f.borrow_mut().stat(b"g").unwrap().0,
            0,
            "failed write published a mapping"
        );
    }

    #[test]
    fn fragmented_random_io_and_reuse() {
        let d = RamDisk::new(4096, 2 << 20);
        let f = vol(&d);
        for i in 0..32u8 {
            write(&f, &[b'f', i], 0, &pattern(4096 * (1 + i as usize % 3), i)).unwrap();
        }
        for i in (0..32u8).step_by(2) {
            f.borrow_mut().remove(&[b'f', i], false).unwrap();
        }
        let big = pattern(40 * 4096, 99);
        write(&f, b"big", 0, &big).unwrap();
        must_read(&f, b"big", &big);
        for i in (1..32u8).step_by(2) {
            must_read(&f, &[b'f', i], &pattern(4096 * (1 + i as usize % 3), i));
        }
    }

    #[test]
    fn hole_fill_merges_both_neighbors() {
        let d = RamDisk::new(4096, 1 << 20);
        let f = vol(&d);
        write(&f, b"h", 0, &[1; 4096]).unwrap();
        write(&f, b"h", 2 * 4096, &[3; 4096]).unwrap();
        write(&f, b"h", 4096, &[2; 4096]).unwrap();
        let n = f.borrow_mut().walk(&split(b"h").unwrap(), false).unwrap();
        // Bump-allocatie: fysiek 0, 1, 2 voor logisch 0, 2, 1: geen buren.
        assert_eq!(f.borrow().node(n).unwrap().extents.len(), 3);
        assert_eq!(f.borrow().index, 3);
        // Aansluitend schrijven smelt samen tot één extent.
        write(&f, b"m", 0, &[4; 4096]).unwrap();
        write(&f, b"m", 4096, &[5; 4096]).unwrap();
        let m = f.borrow_mut().walk(&split(b"m").unwrap(), false).unwrap();
        assert_eq!(f.borrow().node(m).unwrap().extents.len(), 1);
        assert_eq!(f.borrow().index, 4);
        let mut got = [0u8; 3 * 4096];
        on(path::read(&f, b"h", 0, &mut got)).unwrap();
        assert!(got[..4096].iter().all(|b| *b == 1) && got[8192..].iter().all(|b| *b == 3));
    }

    fn persist_disk() -> RamDisk {
        RamDisk::new(512, 64 << 20)
    }

    fn mount(d: &RamDisk, fresh: bool) -> (Vol<'_>, Mounted) {
        let (f, m) = on(Fs::mount(d, 0, d.blocks(), 512, 128 << 10, fresh)).unwrap();
        (LocalCell::cell(f), m)
    }

    #[test]
    fn persist_round_trip() {
        let d = persist_disk();
        let film = pattern((3 << 20) + 123, 1);
        let sub = pattern(5000, 9);
        let (nodes, index) = {
            let (f, m) = mount(&d, false);
            assert_eq!(m, Mounted::Empty);
            write(&f, b"media/Films/A/a.mkv", 0, &film).unwrap();
            write(&f, b"media/Films/A/a.srt", 0, &sub).unwrap();
            f.borrow_mut().mkdir_all(b"media/Backups").unwrap();
            on(path::truncate(&f, b"media/sparse", 10 << 20)).unwrap();
            on(commit(&f)).unwrap();
            (f.borrow().count, f.borrow().index)
        };
        let (g, m) = mount(&d, false);
        assert_eq!(
            m,
            Mounted::Restored {
                generation: 1,
                slot: 0
            }
        );
        must_read(&g, b"media/Films/A/a.mkv", &film);
        must_read(&g, b"media/Films/A/a.srt", &sub);
        assert_eq!(g.borrow_mut().stat(b"media/Backups").unwrap(), (0, true));
        assert_eq!(g.borrow_mut().stat(b"media/sparse").unwrap().0, 10 << 20);
        assert_eq!((g.borrow().count, g.borrow().index), (nodes, index));
        write(&g, b"media/new.bin", 0, &pattern(2 << 20, 5)).unwrap();
        must_read(&g, b"media/Films/A/a.mkv", &film);
    }

    #[test]
    fn persist_torn_slot_falls_back() {
        let d = persist_disk();
        let one = pattern(8192, 1);
        let slot = {
            let (f, _) = mount(&d, false);
            write(&f, b"one", 0, &one).unwrap();
            on(commit(&f)).unwrap();
            write(&f, b"two", 0, &pattern(8192, 2)).unwrap();
            on(commit(&f)).unwrap();
            assert_eq!((f.borrow().last, f.borrow().generation), (Some(1), 2));
            f.borrow().slot
        };
        d.data.borrow_mut()[((u64::from(slot) + 1) * BS + 3) as usize] ^= 0xff;
        let (g, m) = mount(&d, false);
        assert_eq!(
            m,
            Mounted::Restored {
                generation: 1,
                slot: 0
            }
        );
        must_read(&g, b"one", &one);
        assert_eq!(g.borrow_mut().stat(b"two"), Err(Error::NoEnt));
        write(&g, b"three", 0, &pattern(100, 3)).unwrap();
        on(commit(&g)).unwrap();
        assert_eq!((g.borrow().last, g.borrow().generation), (Some(1), 2));
    }

    #[test]
    fn persist_deferred_free() {
        let d = persist_disk();
        let old = pattern(1 << 20, 4);
        let (f, _) = mount(&d, false);
        write(&f, b"old", 0, &old).unwrap();
        on(commit(&f)).unwrap();
        let n = f.borrow_mut().walk(&split(b"old").unwrap(), false).unwrap();
        let run = f.borrow().node(n).unwrap().extents[0];
        f.borrow_mut().remove(b"old", false).unwrap();
        write(&f, b"new", 0, &pattern(4 << 20, 6)).unwrap();
        let n = f.borrow_mut().walk(&split(b"new").unwrap(), false).unwrap();
        for e in &f.borrow().node(n).unwrap().extents {
            assert!(
                !(e.physical < run.physical + run.count && run.physical < e.physical + e.count),
                "new reused old's blocks before a commit"
            );
        }
        on(commit(&f)).unwrap();
        assert!(f.borrow().pending.is_empty());
        assert!(
            f.borrow()
                .free
                .first()
                .is_some_and(|r| r.start == run.physical && r.count >= run.count)
        );
    }

    #[test]
    fn persist_deferred_free_survives_power_loss() {
        let d = persist_disk();
        let old = pattern(1 << 20, 4);
        {
            let (f, _) = mount(&d, false);
            write(&f, b"old", 0, &old).unwrap();
            on(commit(&f)).unwrap();
            f.borrow_mut().remove(b"old", false).unwrap();
            write(&f, b"new", 0, &pattern(4 << 20, 6)).unwrap();
        } // Stroom weg vóór de commit.
        let (g, _) = mount(&d, false);
        must_read(&g, b"old", &old);
    }

    #[test]
    fn persist_other_window_ignored() {
        let d = persist_disk();
        {
            let (f, _) = mount(&d, false);
            write(&f, b"x", 0, &pattern(4096, 1)).unwrap();
            on(commit(&f)).unwrap();
        }
        let blocks = d.blocks() / 2;
        let (g, m) = on(Fs::mount(&d, 0, blocks, 512, 128 << 10, false)).unwrap();
        assert_eq!((m, g.count), (Mounted::Empty, 0));
    }

    #[test]
    fn persist_fresh_clears() {
        let d = persist_disk();
        {
            let (f, _) = mount(&d, false);
            write(&f, b"x", 0, &pattern(4096, 1)).unwrap();
            on(commit(&f)).unwrap();
            write(&f, b"y", 0, &pattern(4096, 2)).unwrap();
            on(commit(&f)).unwrap();
        }
        assert_eq!(mount(&d, true).1, Mounted::Fresh);
        let (g, m) = mount(&d, false);
        assert_eq!((m, g.borrow().count), (Mounted::Empty, 0));
    }

    #[test]
    fn persist_rejects_double_owner() {
        let d = persist_disk();
        {
            let (f, _) = mount(&d, false);
            write(&f, b"a", 0, &pattern(4096, 1)).unwrap();
            write(&f, b"b", 0, &pattern(4096, 2)).unwrap();
            let a = f.borrow_mut().walk(&split(b"a").unwrap(), false).unwrap();
            let b = f.borrow_mut().walk(&split(b"b").unwrap(), false).unwrap();
            let p = f.borrow().node(a).unwrap().extents[0].physical;
            f.borrow_mut().node_mut(b).unwrap().extents[0].physical = p;
            f.borrow_mut().dirty = true;
            on(commit(&f)).unwrap();
        }
        // Een geldige boom die niet ontcijfert, is een fout, geen lege boom
        // die de oudere plek bij de eerste commit overschrijft (F12).
        let r = on(Fs::mount(&d, 0, d.blocks(), 512, 128 << 10, false));
        assert!(r.is_err(), "an inconsistent tree mounted");
    }

    #[test]
    fn a_window_too_small_for_two_places_does_not_mount() {
        let d = RamDisk::new(512, 64 << 10);
        let r = on(Fs::mount(&d, 0, d.blocks(), 512, 128 << 10, false));
        assert!(matches!(r, Err(Error::DiskFull { .. })));
    }

    #[test]
    fn a_block_error_in_the_mount_is_an_error() {
        // Een venster van 64 MiB op een schijf van 1 MiB: de kop van de
        // tweede plek (blok 256) ligt achter de schijf, de lees faalt.
        let d = RamDisk::new(512, 1 << 20);
        let r = on(Fs::mount(&d, 0, (64 << 20) / 512, 512, 128 << 10, false));
        assert!(matches!(r, Err(Error::Io { .. })));
    }
}

#[cfg(test)]
mod sync_tests;

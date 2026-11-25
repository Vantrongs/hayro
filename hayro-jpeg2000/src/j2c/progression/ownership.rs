use super::{PrecinctStore, ProgressionData, cprl, pcrl, rpcl};
use crate::cancel::Cancel;
use crate::error::{DecodingError, Result};
use crate::j2c::codestream::{ProgressionChange, ProgressionOrder, ProgressionOrder::*};
use crate::j2c::tile::{ComponentTile, ResolutionTile, Tile};
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BinaryHeap};
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::ops::Range;

type Shape = (u64, u16);
type ShapeEntry = (Shape, Vec<u32>);
const MAX_SHAPE_CACHE_BYTES: usize = 1024 * 1024;

pub(in crate::j2c) struct OwnershipIndex {
    applicable: Box<[ShapeEntry]>,
    full: Shape,
    entries: Vec<EntryRuns>,
    runs: Vec<BandRun>,
    positional: Vec<PositionalBand>,
}

struct EntryRuns {
    min_layer: u8,
    min_res: u8,
    order: ProgressionOrder,
    runs: Range<usize>,
}

#[derive(Clone, Copy)]
struct BandRun {
    c_start: u16,
    c_end: u16,
    resolution: u8,
    l_start: u8,
    l_end: u8,
}

#[derive(Clone, Copy)]
struct PositionalBand {
    c_start: u16,
    c_end: u16,
    resolution: u8,
}

#[derive(Default)]
struct Intervals(BTreeMap<u16, u16>);

impl Intervals {
    fn insert(&mut self, mut start: u16, mut end: u16) {
        if let Some((&left, &right)) = self.0.range(..=start).next_back()
            && right >= start
        {
            if right >= end {
                return;
            }
            self.0.remove(&left);
            start = left;
        }
        while let Some((&left, &right)) = self.0.range(start..=end).next() {
            self.0.remove(&left);
            end = end.max(right);
        }
        self.0.insert(start, end);
    }

    fn take_overlap(&mut self, start: u16, end: u16) -> Option<(u16, u16)> {
        let (&left, &right) = self
            .0
            .range(..=start)
            .next_back()
            .filter(|&(_, &right)| right > start)
            .or_else(|| self.0.range(start..end).next())?;
        self.0.remove(&left);
        if left < start {
            self.0.insert(left, start);
        }
        if right > end {
            self.0.insert(end, right);
        }
        Some((left.max(start), right.min(end)))
    }
}

#[derive(Default)]
struct Row {
    groups: BTreeMap<u8, Intervals>,
    uncovered: Intervals,
    covered: Intervals,
}

type Packets<'a> = Box<dyn Iterator<Item = Result<ProgressionData>> + 'a>;

fn component_groups(
    tile: &Tile<'_>,
    cancel: Cancel<'_>,
    mut visit: impl FnMut(u16, u16, ComponentTile<'_>),
) -> Result<()> {
    let mut c = 0;
    while c < tile.component_infos.len() {
        cancel.check()?;
        let start = c as u16;
        let info = &tile.component_infos[c];
        let shape = |info: &crate::j2c::codestream::ComponentInfo| {
            (
                info.size_info.horizontal_resolution,
                info.size_info.vertical_resolution,
                info.num_resolution_levels(),
            )
        };
        let key = shape(info);
        c += 1;
        while c < tile.component_infos.len() && shape(&tile.component_infos[c]) == key {
            if c % 1024 == 0 {
                cancel.check()?;
            }
            c += 1;
        }
        let end = c as u16;
        visit(start, end, ComponentTile::new(tile, info));
    }
    Ok(())
}

fn applies(shape: Shape, runs: &[BandRun], cancel: Cancel<'_>) -> Result<bool> {
    for (i, run) in runs.iter().enumerate() {
        if i > 0 && i % 1024 == 0 {
            cancel.check()?;
        }
        if shape.0 & (1_u64 << run.resolution) != 0 && u16::from(run.l_start) < shape.1 {
            return Ok(true);
        }
    }
    Ok(false)
}

impl OwnershipIndex {
    pub(in crate::j2c) fn new<'t>(
        changes: &[ProgressionChange],
        tiles: impl Iterator<Item = &'t Tile<'t>>,
        cancel: Cancel<'_>,
    ) -> Result<Self> {
        let mut rows = Vec::<Row>::new();
        let mut max_layers = 0;
        let mut positional = false;
        for chunk in changes.chunks(1024) {
            cancel.check()?;
            if chunk.iter().any(|p| {
                !matches!(
                    p.progression_order,
                    LayerResolutionComponentPosition | ResolutionLayerComponentPosition
                )
            }) {
                positional = true;
                break;
            }
        }
        let mut shapes = BTreeMap::new();
        let mut full_mask = 0;
        for tile in tiles {
            cancel.check()?;
            max_layers = max_layers.max(tile.num_layers);
            let mut mask = 0;
            component_groups(tile, cancel, |start, end, component| {
                let depth = component.component_info.num_resolution_levels();
                if rows.len() < usize::from(depth) {
                    rows.resize_with(usize::from(depth), Row::default);
                }
                for (r, row) in rows[..usize::from(depth)].iter_mut().enumerate() {
                    if positional {
                        row.uncovered.insert(start, end);
                    }
                    if ResolutionTile::new(component, r as u8).num_precincts() != 0 {
                        mask |= 1_u64 << r;
                        row.groups.entry(0).or_default().insert(start, end);
                    }
                }
            })?;
            full_mask |= mask;
            *shapes
                .entry((mask, u16::from(tile.num_layers)))
                .or_insert(0_usize) += 1;
        }

        let mut entries = Vec::new();
        let mut runs = Vec::new();
        for p in changes {
            cancel.check()?;
            let start = p.component_start;
            let end = p.component_end;
            if start >= end {
                continue;
            }
            let layer = p.layer_end.min(u16::from(max_layers)) as u8;
            let first = runs.len();
            let mut min_layer = layer;
            for r in p.resolution_start..p.resolution_end.min(rows.len() as u8) {
                let row = &mut rows[usize::from(r)];
                if !matches!(
                    p.progression_order,
                    LayerResolutionComponentPosition | ResolutionLayerComponentPosition
                ) {
                    while let Some((c_start, c_end)) = row.uncovered.take_overlap(start, end) {
                        cancel.check()?;
                        row.covered.insert(c_start, c_end);
                    }
                }
                let row_start = runs.len();
                for (&h, intervals) in row.groups.range_mut(..layer) {
                    while let Some((c_start, c_end)) = intervals.take_overlap(start, end) {
                        cancel.check()?;
                        min_layer = min_layer.min(h);
                        runs.push(BandRun {
                            c_start,
                            c_end,
                            resolution: r,
                            l_start: h,
                            l_end: layer,
                        });
                    }
                }
                if row_start != runs.len() {
                    row.groups.retain(|_, v| !v.0.is_empty());
                    let dest = row.groups.entry(layer).or_default();
                    runs[row_start..].sort_unstable_by_key(|x| x.c_start);
                    for run in &runs[row_start..] {
                        dest.insert(run.c_start, run.c_end);
                    }
                }
            }
            if first != runs.len() {
                entries.push(EntryRuns {
                    order: p.progression_order,
                    min_layer,
                    min_res: runs[first].resolution,
                    runs: first..runs.len(),
                });
            }
        }
        let mut positional = Vec::new();
        for (r, row) in rows.into_iter().enumerate() {
            for (c_start, c_end) in row.covered.0 {
                cancel.check()?;
                positional.push(PositionalBand {
                    c_start,
                    c_end,
                    resolution: r as u8,
                });
            }
        }
        let full = (full_mask, u16::from(max_layers));
        let mut candidates: Vec<_> = shapes
            .into_iter()
            .filter(|&(shape, count)| shape != full && count >= 2)
            .collect();
        candidates.sort_unstable_by_key(|&(_, count)| core::cmp::Reverse(count));
        let mut remaining = MAX_SHAPE_CACHE_BYTES;
        let mut applicable = Vec::new();
        for (shape, _) in candidates {
            let Some(payload_bytes) = remaining.checked_sub(size_of::<ShapeEntry>()) else {
                break;
            };
            let max_indices = payload_bytes / size_of::<u32>();
            let mut count = 0;
            for (i, entry) in entries.iter().enumerate() {
                if i % 1024 == 0 {
                    cancel.check()?;
                }
                if applies(shape, &runs[entry.runs.clone()], cancel)? {
                    count += 1;
                    if count > max_indices {
                        break;
                    }
                }
            }
            if count > max_indices || count == entries.len() {
                continue;
            }
            let mut list = Vec::with_capacity(count);
            for (i, entry) in entries.iter().enumerate() {
                if i % 1024 == 0 {
                    cancel.check()?;
                }
                if applies(shape, &runs[entry.runs.clone()], cancel)? {
                    list.push(i as u32);
                }
            }
            remaining -= size_of::<ShapeEntry>() + list.capacity() * size_of::<u32>();
            applicable.push((shape, list));
        }
        applicable.sort_unstable_by_key(|&(shape, _)| shape);
        Ok(Self {
            full,
            applicable: applicable.into_boxed_slice(),
            entries,
            runs,
            positional,
        })
    }

    pub(super) fn cached(&self, shape: Shape) -> Option<&[u32]> {
        self.applicable
            .binary_search_by_key(&shape, |&(key, _)| key)
            .ok()
            .map(|i| self.applicable[i].1.as_slice())
    }

    fn validate_positional(&self, tile: &Tile<'_>, depth: u8, cancel: Cancel<'_>) -> Result<()> {
        let tile_components = tile.component_infos.len();
        for band in self.positional.iter().take_while(|b| b.resolution < depth) {
            let c_end = (band.c_end as usize).min(tile_components);
            for c in (band.c_start as usize)..c_end {
                cancel.check()?;
                let info = &tile.component_infos[c];
                if band.resolution < info.num_resolution_levels()
                    && ResolutionTile::new(ComponentTile::new(tile, info), band.resolution)
                        .precincts()
                        .is_none()
                {
                    return Err(DecodingError::InvalidProgressionIterator.into());
                }
            }
        }
        Ok(())
    }

    pub(in crate::j2c) fn packets<'t>(
        &'t self,
        tile: &'t Tile<'t>,
        cancel: Cancel<'t>,
    ) -> Result<Packets<'t>> {
        let iter = PocPacketIter::new(tile, self, cancel)?;
        self.validate_positional(tile, iter.tile_max_res, cancel)?;
        Ok(Box::new(iter))
    }

    #[cfg(test)]
    pub(super) fn run_count(&self) -> usize {
        self.runs.len()
    }

    #[cfg(test)]
    pub(super) fn estimated_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<EntryRuns>()
            + self.runs.capacity() * size_of::<BandRun>()
            + self.positional.capacity() * size_of::<PositionalBand>()
            + size_of_val(&*self.applicable)
            + self
                .applicable
                .iter()
                .map(|(_, list)| list.capacity() * size_of::<u32>())
                .sum::<usize>()
    }
}

struct PocPacketIter<'t> {
    tile: &'t Tile<'t>,
    tile_max_res: u8,
    index: &'t OwnershipIndex,
    applicable: Option<&'t [u32]>,
    shape: Shape,
    cancel: Cancel<'t>,
    next_entry: usize,
    active: ActiveEntry,
}

impl<'t> PocPacketIter<'t> {
    fn new(tile: &'t Tile<'t>, index: &'t OwnershipIndex, cancel: Cancel<'t>) -> Result<Self> {
        let mut tile_max_res = 0;
        let mut mask = 0;
        component_groups(tile, cancel, |_, _, component| {
            tile_max_res = tile_max_res.max(component.component_info.num_resolution_levels());
            for r in component.resolution_tiles() {
                if r.num_precincts() > 0 {
                    mask |= 1_u64 << r.resolution;
                }
            }
        })?;
        let shape = (mask, u16::from(tile.num_layers));
        Ok(Self {
            tile,
            tile_max_res,
            index,
            applicable: index.cached(shape),
            shape,
            cancel,
            next_entry: 0,
            active: ActiveEntry::None,
        })
    }
}

impl PocPacketIter<'_> {
    fn pull(&mut self) -> Result<Option<ProgressionData>> {
        loop {
            let packet = match &mut self.active {
                ActiveEntry::None => None,
                ActiveEntry::LayerRes(iter) => {
                    iter.next(self.tile, &self.index.runs, self.cancel)?
                }
                ActiveEntry::Positional(iter) => iter.next(),
            };
            if packet.is_some() {
                return Ok(packet);
            }
            self.cancel.check()?;
            let entry = match self.applicable {
                Some(list) => list
                    .get(self.next_entry)
                    .map(|&i| &self.index.entries[i as usize]),
                None => self.index.entries.get(self.next_entry),
            };
            let Some(entry) = entry else { return Ok(None) };
            self.next_entry += 1;
            self.active = ActiveEntry::None;
            if self.applicable.is_none()
                && self.shape != self.index.full
                && !applies(
                    self.shape,
                    &self.index.runs[entry.runs.clone()],
                    self.cancel,
                )?
            {
                continue;
            }
            self.active = ActiveEntry::new(
                self.tile,
                self.tile_max_res,
                entry,
                &self.index.runs,
                self.cancel,
            )?;
        }
    }
}

impl Iterator for PocPacketIter<'_> {
    type Item = Result<ProgressionData>;
    fn next(&mut self) -> Option<Self::Item> {
        self.pull().transpose()
    }
}

enum ActiveEntry {
    None,
    LayerRes(LayerResIter),
    Positional(PositionalIter),
}

impl ActiveEntry {
    fn new(
        tile: &Tile<'_>,
        tile_max_res: u8,
        entry: &EntryRuns,
        all_runs: &[BandRun],
        cancel: Cancel<'_>,
    ) -> Result<Self> {
        Ok(match entry.order {
            LayerResolutionComponentPosition | ResolutionLayerComponentPosition => {
                Self::LayerRes(LayerResIter::new(tile, tile_max_res, entry, all_runs))
            }
            _ => Self::Positional(PositionalIter::new(
                tile,
                &all_runs[entry.runs.clone()],
                entry.order,
                cancel,
            )?),
        })
    }
}

struct LayerResIter {
    runs: Range<usize>,
    pairs: Range<u16>,
    resolutions: Range<u8>,
    layers: Range<u8>,
    layer_first: bool,
    row: Range<usize>,
    components: Range<u16>,
    precincts: Range<u64>,
    packet: ProgressionData,
}

impl LayerResIter {
    fn new(tile: &Tile<'_>, depth: u8, entry: &EntryRuns, runs: &[BandRun]) -> Self {
        let last = runs[entry.runs.end - 1];
        let resolutions = entry.min_res..depth.min(last.resolution + 1);
        let layers = entry.min_layer..tile.num_layers.min(last.l_end);
        Self {
            pairs: 0..(resolutions.len() * layers.len()) as u16,
            runs: entry.runs.clone(),
            resolutions,
            layers,
            layer_first: matches!(entry.order, LayerResolutionComponentPosition),
            row: 0..0,
            components: 0..0,
            precincts: 0..0,
            packet: ProgressionData::default(),
        }
    }

    fn next(
        &mut self,
        tile: &Tile<'_>,
        runs: &[BandRun],
        cancel: Cancel<'_>,
    ) -> Result<Option<ProgressionData>> {
        let mut steps = 0;
        loop {
            if let Some(precinct) = self.precincts.next() {
                return Ok(Some(ProgressionData {
                    precinct,
                    ..self.packet
                }));
            }
            steps += 1;
            if steps % 1024 == 0 {
                cancel.check()?;
            }
            if let Some(c) = self.components.next() {
                let info = &tile.component_infos[usize::from(c)];
                if self.packet.resolution < info.num_resolution_levels() {
                    self.packet.component = c;
                    self.precincts = 0..ResolutionTile::new(
                        ComponentTile::new(tile, info),
                        self.packet.resolution,
                    )
                    .num_precincts();
                }
            } else if let Some(i) = self.row.next() {
                let run = runs[i];
                if self.packet.layer_num >= run.l_start {
                    self.components = run.c_start..run.c_end.min(tile.component_infos.len() as u16);
                }
            } else if let Some(pair) = self.pairs.next() {
                let (l, r) = if self.layer_first {
                    let width = self.resolutions.len() as u16;
                    (pair / width, pair % width)
                } else {
                    let width = self.layers.len() as u16;
                    (pair % width, pair / width)
                };
                self.packet.layer_num = self.layers.start + l as u8;
                self.packet.resolution = self.resolutions.start + r as u8;
                let row = &runs[self.runs.clone()];
                let start = self.runs.start
                    + row.partition_point(|run| run.resolution < self.packet.resolution);
                let end = self.runs.start
                    + row.partition_point(|run| run.resolution <= self.packet.resolution);
                self.row = start..end;
            } else {
                return Ok(None);
            }
        }
    }
}

struct Position {
    store: PrecinctStore,
    layers: Range<u8>,
    order: ProgressionOrder,
}

impl Ord for Position {
    fn cmp(&self, other: &Self) -> Ordering {
        let compare = match self.order {
            ResolutionPositionComponentLayer => rpcl,
            PositionComponentResolutionLayer => pcrl,
            ComponentPositionResolutionLayer => cprl,
            _ => unreachable!(),
        };
        (self.order as u8)
            .cmp(&(other.order as u8))
            .then_with(|| compare(&other.store, &self.store))
    }
}

impl PartialOrd for Position {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl PartialEq for Position {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}
impl Eq for Position {}

struct PositionalIter {
    positions: BinaryHeap<Position>,
    current: Option<Position>,
}

impl PositionalIter {
    fn new(
        tile: &Tile<'_>,
        runs: &[BandRun],
        order: ProgressionOrder,
        cancel: Cancel<'_>,
    ) -> Result<Self> {
        let mut positions = BinaryHeap::new();
        let tile_components = tile.component_infos.len();
        for &run in runs {
            cancel.check()?;
            let l_end = run.l_end.min(tile.num_layers);
            if run.l_start >= l_end {
                continue;
            }
            let c_end = (run.c_end as usize).min(tile_components);
            for c in (run.c_start as usize)..c_end {
                if c % 1024 == 0 {
                    cancel.check()?;
                }
                let info = &tile.component_infos[c];
                let r = run.resolution;
                if r < info.num_resolution_levels() {
                    let component = ComponentTile::new(tile, info);
                    let res_tile = ResolutionTile::new(component, r);
                    if res_tile.num_precincts() == 0 {
                        continue;
                    }
                    for p in res_tile
                        .precincts()
                        .expect("positional layouts were validated")
                    {
                        if positions.len() % 1024 == 0 {
                            cancel.check()?;
                        }
                        positions.push(Position {
                            store: PrecinctStore {
                                resolution: r,
                                precinct_y: p.r_y,
                                precinct_x: p.r_x,
                                component_idx: c as u16,
                                precinct_idx: p.idx,
                            },
                            layers: run.l_start..l_end,
                            order,
                        });
                    }
                }
            }
        }
        Ok(Self {
            positions,
            current: None,
        })
    }

    fn next(&mut self) -> Option<ProgressionData> {
        loop {
            if let Some(Position { store, layers, .. }) = &mut self.current
                && let Some(layer_num) = layers.next()
            {
                return Some(ProgressionData {
                    layer_num,
                    resolution: store.resolution,
                    component: store.component_idx,
                    precinct: store.precinct_idx,
                });
            }
            self.current = Some(self.positions.pop()?);
        }
    }
}

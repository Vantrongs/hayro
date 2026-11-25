use crate::j2c::codestream::ProgressionChange;
use crate::{DecodeSettings, DecoderContext, Image};
use alloc::vec;
use alloc::vec::Vec;

fn marker(code: u16, body: &[u8]) -> Vec<u8> {
    let mut marker = code.to_be_bytes().to_vec();
    marker.extend(u16::try_from(body.len() + 2).unwrap().to_be_bytes());
    marker.extend(body);
    marker
}

fn position_order_stream(components: u8, levels: u8, side: u32, tile_header: &[u8]) -> Vec<u8> {
    let mut siz = vec![0, 0];
    for value in [side, side, 0, 0, side, side, 0, 0] {
        siz.extend(value.to_be_bytes());
    }
    siz.extend(u16::from(components).to_be_bytes());
    siz.extend([7, 1, 1].repeat(usize::from(components)));
    let mut cod = vec![1, 2, 0, 1, 0, levels, 4, 4, 0, 1];
    cod.extend(vec![0x11; usize::from(levels) + 1]);
    let mut qcd = vec![0x40];
    qcd.extend(vec![0x48; 3 * usize::from(levels) + 1]);
    let change = [0, 0, 0, 1, levels + 1, components, 2];
    let packets = usize::from(components) * (usize::from(levels) + 1);
    let data = vec![0; packets + 1];
    let mut sot = vec![0, 0];
    sot.extend(
        u32::try_from(12 + tile_header.len() + 2 + data.len())
            .unwrap()
            .to_be_bytes(),
    );
    sot.extend([0, 1]);

    let mut stream = vec![0xFF, 0x4F];
    stream.extend(marker(0xFF51, &siz));
    stream.extend(marker(0xFF52, &cod));
    stream.extend(marker(0xFF5C, &qcd));
    stream.extend(marker(0xFF5F, &change));
    stream.extend(marker(0xFF90, &sot));
    stream.extend(tile_header);
    stream.extend([0xFF, 0x93]);
    stream.extend(data);
    stream.extend([0xFF, 0xD9]);
    stream
}

#[test]
fn invalid_precinct_in_a_position_order_change() {
    use super::ProgressionOrder::*;
    use crate::{DecodeError, DecodingError};

    let data = position_order_stream(1, 1, 1, &[]);
    let mut image = Image::new(&data, &DecodeSettings::default()).unwrap();
    image.header.component_infos[0]
        .coding_style
        .parameters
        .precinct_exponents[1]
        .0 = 0;
    let change = &mut image.header.progression_changes[0];
    change.progression_order = LayerResolutionComponentPosition;
    let mut positional = *change;
    positional.progression_order = ResolutionPositionComponentLayer;

    for (end, expected) in [
        (1, DecodingError::InvalidPrecinct),
        (2, DecodingError::InvalidProgressionIterator),
    ] {
        positional.resolution_end = end;
        image.header.progression_changes.truncate(1);
        image.header.progression_changes.push(positional);
        assert_eq!(
            image.decode(&mut DecoderContext::default()).err(),
            Some(DecodeError::Decoding(expected))
        );
    }
}

#[test]
fn overflowing_stride_in_an_empty_resolution() {
    use crate::{DecodeError, DecodingError};

    let data = position_order_stream(1, 17, 2, &[]);
    let mut image = Image::new(&data, &DecodeSettings::default()).unwrap();
    image.header.size_data.image_area_x_offset = 1;
    image.header.component_infos[0]
        .coding_style
        .parameters
        .precinct_exponents[0]
        .0 = 15;
    for (start, expected) in [
        (0, DecodingError::InvalidProgressionIterator),
        (1, DecodingError::InvalidPrecinct),
    ] {
        image.header.progression_changes[0].resolution_start = start;
        assert_eq!(
            image.decode(&mut DecoderContext::default()).err(),
            Some(DecodeError::Decoding(expected))
        );
    }
}

#[test]
fn positional_layout_precedes_reduced_tile_resolution_error() {
    use crate::{DecodeError, DecodingError};

    let tile_cod = marker(0xff52, &[1, 2, 0, 1, 0, 1, 4, 4, 0, 1, 0x11, 0x10]);
    let data = position_order_stream(1, 2, 4, &tile_cod);
    for strict in [false, true] {
        let settings = DecodeSettings {
            strict,
            target_resolution: Some((1, 1)),
            ..Default::default()
        };
        let image = Image::new(&data, &settings).unwrap();
        assert_eq!(image.header.skipped_resolution_levels, 2);
        assert_eq!(
            image.decode(&mut DecoderContext::default()).err(),
            Some(DecodeError::Decoding(
                DecodingError::InvalidProgressionIterator
            ))
        );
    }
}

#[test]
fn positional_component_indices_do_not_wrap() {
    use crate::{DecodeError, DecodingError};

    let data = position_order_stream(1, 1, 1, &[]);
    let mut image = Image::new(&data, &DecodeSettings::default()).unwrap();
    let infos = &mut image.header.component_infos;
    infos.resize(257, infos[0].clone());
    infos[256].coding_style.parameters.precinct_exponents[1].0 = 0;
    assert_eq!(
        image.decode(&mut DecoderContext::default()).err(),
        Some(DecodeError::Decoding(DecodingError::InvalidPrecinct))
    );
    image.header.progression_changes[0].component_start = 256;
    image.header.progression_changes[0].component_end = 257;
    assert_eq!(
        image.decode(&mut DecoderContext::default()).err(),
        Some(DecodeError::Decoding(
            DecodingError::InvalidProgressionIterator
        ))
    );
    image.header.component_infos[256]
        .coding_style
        .parameters
        .precinct_exponents[1]
        .0 = 1;
    let tiles = crate::j2c::tile::parse(
        &mut crate::reader::BitReader::new(image.codestream),
        &image.header,
    )
    .unwrap();
    let cancel = super::Cancel(None);
    let index = super::OwnershipIndex::new(&image.header.progression_changes, tiles.iter(), cancel)
        .unwrap();
    let mut packets = index.packets(&tiles[0], cancel).unwrap();
    assert_eq!(packets.next().transpose().unwrap().unwrap().component, 256);
}

#[test]
fn cancellation_while_building_a_later_position_order() {
    use super::*;
    use crate::j2c::{parse_raw, tile};
    use crate::reader::BitReader;
    use crate::{CancelHandle, DecodeError};

    let data = position_order_stream(2, 1, 16, &[]);
    let stream = parse_raw(&data, &DecodeSettings::default()).unwrap();
    let mut tiles = tile::parse(&mut BitReader::new(stream.data), &stream.header).unwrap();
    let tile = &mut tiles[0];
    let all = stream.header.progression_changes[0];
    tile.progression_changes = vec![
        ProgressionChange {
            component_end: 1,
            resolution_end: 1,
            progression_order: ProgressionOrder::LayerResolutionComponentPosition,
            ..all
        },
        ProgressionChange {
            component_start: 1,
            ..all
        },
    ];
    let index =
        OwnershipIndex::new(&tile.progression_changes, iter::once(&*tile), Cancel(None)).unwrap();
    let flag = crate::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
    let signal = flag.clone();
    let handle = CancelHandle::new(move || signal.load(core::sync::atomic::Ordering::Relaxed));
    let mut packets = index.packets(tile, Cancel(Some(&handle))).unwrap();
    let first = ResolutionTile::new(ComponentTile::new(tile, &tile.component_infos[0]), 0);
    for _ in 0..first.num_precincts() {
        assert_eq!(packets.next().transpose().unwrap().unwrap().component, 0);
    }
    flag.store(true, core::sync::atomic::Ordering::Relaxed);
    assert_eq!(packets.next().transpose(), Err(DecodeError::Cancelled));
}

#[test]
fn cancellation_while_skipping_changes_and_packets() {
    use super::*;
    use crate::j2c::{parse_raw, tile};
    use crate::reader::BitReader;
    use crate::{CancelHandle, DecodeError};

    let data = position_order_stream(2, 1, 1, &[]);
    let stream = parse_raw(&data, &DecodeSettings::default()).unwrap();
    let mut tiles = tile::parse(&mut BitReader::new(stream.data), &stream.header).unwrap();
    let tile = &mut tiles[0];
    tile.num_layers = 2;
    let first = ProgressionChange {
        component_end: 1,
        progression_order: ProgressionOrder::LayerResolutionComponentPosition,
        ..stream.header.progression_changes[0]
    };

    for changes in [
        vec![first; 3],
        vec![
            first,
            ProgressionChange {
                layer_end: 2,
                ..first
            },
        ],
    ] {
        tile.progression_changes = changes;
        let index =
            OwnershipIndex::new(&tile.progression_changes, iter::once(&*tile), Cancel(None))
                .unwrap();
        let flag = crate::sync::Arc::new(core::sync::atomic::AtomicBool::new(false));
        let signal = flag.clone();
        let handle = CancelHandle::new(move || signal.load(core::sync::atomic::Ordering::Relaxed));
        let mut packets = index.packets(tile, Cancel(Some(&handle))).unwrap();
        for _ in 0..2 {
            assert!(packets.next().transpose().unwrap().is_some());
        }
        flag.store(true, core::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            packets.next().transpose(),
            Err(DecodeError::Cancelled)
        ));
    }
}

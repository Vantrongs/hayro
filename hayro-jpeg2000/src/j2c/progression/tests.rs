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

fn multi_tile_stream() -> Vec<u8> {
    // 2x2 grid of 16x16 tiles over a non-zero offset image with 3 subsampled components,
    // main-header POC, and a tile-header POC override on tile 1.
    let mut siz = vec![0, 0];
    for value in [32_u32, 32, 1, 2, 16, 16, 0, 0] {
        siz.extend(value.to_be_bytes());
    }
    siz.extend(3_u16.to_be_bytes());
    siz.extend([7, 1, 1, 7, 2, 1, 7, 1, 2]);
    let levels = 3_u8;
    let mut cod = vec![1, 0, 0, 6, 0, levels, 4, 4, 0, 1];
    cod.extend([0x11, 0x22, 0x22, 0x22]);
    let mut qcd = vec![0x40];
    qcd.extend(vec![0x48; 3 * usize::from(levels) + 1]);
    let main_poc = [0, 0, 0, 6, levels + 1, 3, 0];
    let tile_override_poc = marker(0xFF5F, &[0, 0, 0, 2, 2, 3, 2, 0, 0, 0, 6, 8, 3, 4]);

    let mut stream = vec![0xFF, 0x4F];
    stream.extend(marker(0xFF51, &siz));
    stream.extend(marker(0xFF52, &cod));
    stream.extend(marker(0xFF5C, &qcd));
    stream.extend(marker(0xFF5F, &main_poc));
    for tile_idx in 0_u16..4 {
        let tile_header = if tile_idx == 1 {
            &tile_override_poc[..]
        } else {
            &[]
        };
        let data = [0_u8; 4];
        let mut sot = tile_idx.to_be_bytes().to_vec();
        sot.extend(
            u32::try_from(12 + tile_header.len() + 2 + data.len())
                .unwrap()
                .to_be_bytes(),
        );
        sot.extend([0, 1]);
        stream.extend(marker(0xFF90, &sot));
        stream.extend(tile_header);
        stream.extend([0xFF, 0x93]);
        stream.extend(data);
    }
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
fn high_origin_empty_resolution_does_not_overflow() {
    use crate::j2c::codestream::{ProgressionChange, ProgressionOrder::*};
    use crate::{DecodeError, DecodingError};

    // 2x2 image at the upper end of the u32 reference grid with 2 decompositions:
    // resolution 0 rounds origin up to 2^30 (empty) and scaling back by 2^2 reaches 2^32.
    let origin = u32::MAX - 2;
    for (x_origin, x_end, y_origin, y_end) in [
        (origin, u32::MAX, 0, 2),
        (0, 2, origin, u32::MAX),
        (origin, u32::MAX, origin, u32::MAX),
    ] {
        let mut siz = vec![0, 0];
        for value in [x_end, y_end, x_origin, y_origin, 2, 2, x_origin, y_origin] {
            siz.extend(value.to_be_bytes());
        }
        siz.extend(1_u16.to_be_bytes());
        siz.extend([7, 1, 1]);
        // 2 decompositions -> 3 resolution levels; resolution 1 has zero precinct exponent.
        let cod = [1, 2, 0, 1, 0, 2, 4, 4, 0, 1, 0x11, 0x10, 0x11];
        let mut qcd = vec![0x40];
        qcd.extend([0x48; 7]);
        let data = [0_u8; 4];
        let mut sot = vec![0, 0];
        sot.extend(u32::try_from(12 + 2 + data.len()).unwrap().to_be_bytes());
        sot.extend([0, 1]);

        let mut stream = vec![0xFF, 0x4F];
        stream.extend(marker(0xFF51, &siz));
        stream.extend(marker(0xFF52, &cod));
        stream.extend(marker(0xFF5C, &qcd));
        stream.extend(marker(0xFF90, &sot));
        stream.extend([0xFF, 0x93]);
        stream.extend(data);
        stream.extend([0xFF, 0xD9]);

        let mut image = Image::new(&stream, &DecodeSettings::default()).unwrap();
        // Place the failing resolution 1 POC before the high-origin resolution 0 POC.
        image.header.progression_changes = vec![
            ProgressionChange {
                resolution_start: 1,
                component_start: 0,
                layer_end: 1,
                resolution_end: 2,
                component_end: 1,
                progression_order: ResolutionPositionComponentLayer,
            },
            ProgressionChange {
                resolution_start: 0,
                component_start: 0,
                layer_end: 1,
                resolution_end: 1,
                component_end: 1,
                progression_order: ResolutionPositionComponentLayer,
            },
        ];
        assert_eq!(
            image.decode(&mut DecoderContext::default()).err(),
            Some(DecodeError::Decoding(
                DecodingError::InvalidProgressionIterator
            ))
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
fn overlapping_changes_match_first_occurrence() {
    use super::*;
    use crate::cancel::Cancel;
    use crate::j2c::{codestream::ProgressionChange, tile};
    use crate::reader::BitReader;
    use ProgressionOrder::*;
    use alloc::collections::BTreeSet;

    let stream = multi_tile_stream();
    let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
    let mut tiles = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    assert_eq!(tiles.len(), 4);
    assert!(tiles.iter().any(|t| !t.progression_changes.is_empty()));
    assert!(tiles.iter().any(|t| t.progression_changes.is_empty()));

    for (i, tile) in tiles.iter_mut().enumerate() {
        tile.num_layers = if i % 2 == 0 { 2 } else { 6 };
        let style = &mut tile.component_infos[0].coding_style.parameters;
        style.num_resolution_levels = if i % 2 == 0 { 1 } else { 7 };
        style.num_decomposition_levels = style.num_resolution_levels - 1;
        style
            .precinct_exponents
            .resize(usize::from(style.num_resolution_levels), (3, 3));
    }
    let orders = [
        LayerResolutionComponentPosition,
        ResolutionLayerComponentPosition,
        ResolutionPositionComponentLayer,
        PositionComponentResolutionLayer,
        ComponentPositionResolutionLayer,
    ];
    let cancel = Cancel(None);
    for first in orders {
        for second in orders {
            for (component_start, resolution_start, layer_end) in [
                (0, 0, 1),
                (1, 0, 2),
                (0, 1, 3),
                (1, 1, 4),
                (0, 0, 0),
                (0, 4, 3),
            ] {
                let changes = [
                    ProgressionChange {
                        resolution_start,
                        component_start,
                        layer_end,
                        resolution_end: 8,
                        component_end: 2,
                        progression_order: first,
                    },
                    ProgressionChange {
                        resolution_start: 0,
                        component_start: 0,
                        layer_end: 7,
                        resolution_end: 8,
                        component_end: 256,
                        progression_order: second,
                    },
                ];
                let index = OwnershipIndex::new(
                    &changes,
                    tiles
                        .iter()
                        .filter(|tile| tile.progression_changes.is_empty()),
                    cancel,
                )
                .unwrap();
                for tile in &tiles {
                    let mut seen = BTreeSet::new();
                    let effective = if tile.progression_changes.is_empty() {
                        &changes[..]
                    } else {
                        &tile.progression_changes
                    };
                    let expected: Vec<_> = effective
                        .iter()
                        .flat_map(|p| {
                            let bounds = IteratorInput::new_with_custom_bounds(
                                tile,
                                (p.resolution_start, p.resolution_end),
                                (0, p.layer_end as u8),
                                (p.component_start, p.component_end),
                            );
                            progression_order_iterator(bounds, p.progression_order, Cancel(None))
                                .unwrap()
                        })
                        .filter(|p| seen.insert(*p))
                        .collect();
                    assert_eq!(
                        {
                            let local;
                            let selected = if tile.progression_changes.is_empty() {
                                &index
                            } else {
                                local = OwnershipIndex::new(
                                    &tile.progression_changes,
                                    iter::once(tile),
                                    cancel,
                                )
                                .unwrap();
                                &local
                            };
                            progression_iterator(tile, Some(selected), cancel)
                                .unwrap()
                                .map(Result::unwrap)
                                .collect::<Vec<_>>()
                        },
                        expected
                    );
                }
            }
        }
    }
}

#[test]
fn zero_component_end_includes_component_255() {
    use super::*;
    use crate::j2c::{codestream::poc_marker, tile};
    use crate::{cancel::Cancel, reader::BitReader};

    let stream = position_order_stream(1, 0, 1, &[]);
    let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
    let mut tiles = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    let tile = &mut tiles[0];
    tile.component_infos
        .resize(256, tile.component_infos[0].clone());
    let changes = poc_marker(&mut BitReader::new(&[0, 9, 0, 255, 0, 1, 1, 0, 2]), 256).unwrap();
    assert_eq!(changes[0].component_end, 256);
    let index = OwnershipIndex::new(&changes, iter::once(&*tile), Cancel(None)).unwrap();
    let packets: Vec<_> = index
        .packets(tile, Cancel(None))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        packets,
        [ProgressionData {
            component: 255,
            ..Default::default()
        }]
    );
    use ProgressionOrder::*;
    for order in [
        LayerResolutionComponentPosition,
        ResolutionLayerComponentPosition,
        ResolutionPositionComponentLayer,
        PositionComponentResolutionLayer,
        ComponentPositionResolutionLayer,
    ] {
        tile.progression_order = order;
        let full = [crate::j2c::codestream::ProgressionChange {
            component_start: 0,
            progression_order: order,
            ..changes[0]
        }];
        let index = OwnershipIndex::new(&full, iter::once(&*tile), Cancel(None)).unwrap();
        let plain = progression_iterator(tile, None, Cancel(None))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        assert_eq!(plain.len(), 256);
        assert!(
            index
                .packets(tile, Cancel(None))
                .unwrap()
                .map(Result::unwrap)
                .eq(plain)
        );
    }
}

#[test]
fn two_byte_zero_component_end_includes_component_256() {
    use super::*;
    use crate::j2c::{codestream, tile};
    use crate::{cancel::Cancel, reader::BitReader};

    // Build a 257-component codestream with a 2-byte POC entry: CSpoc = 256, CEpoc = 0.
    let components = 257_u16;
    let mut siz = vec![0, 0];
    for value in [1_u32, 1, 0, 0, 1, 1, 0, 0] {
        siz.extend(value.to_be_bytes());
    }
    siz.extend(components.to_be_bytes());
    siz.extend([7, 1, 1].repeat(usize::from(components)));
    let cod = [1, 2, 0, 1, 0, 0, 4, 4, 0, 1, 0x11];
    let qcd = [0x40, 0x48];
    // 2-byte POC entry (9 bytes): RSpoc=0, CSpoc=256 (0x0100), LYEpoc=1, REpoc=1, CEpoc=0, Ppoc=2.
    let poc = [0, 0x01, 0x00, 0x00, 0x01, 1, 0x00, 0x00, 2];
    let data = [0_u8; 2];
    let mut sot = vec![0, 0];
    sot.extend(u32::try_from(12 + 2 + data.len()).unwrap().to_be_bytes());
    sot.extend([0, 1]);

    let mut stream = vec![0xFF, 0x4F];
    stream.extend(marker(0xFF51, &siz));
    stream.extend(marker(0xFF52, &cod));
    stream.extend(marker(0xFF5C, &qcd));
    stream.extend(marker(0xFF5F, &poc));
    stream.extend(marker(0xFF90, &sot));
    stream.extend([0xFF, 0x93]);
    stream.extend(data);
    stream.extend([0xFF, 0xD9]);

    let mut reader = BitReader::new(&stream);
    assert_eq!(reader.read_marker().unwrap(), codestream::markers::SOC);
    let header = codestream::read_header(&mut reader, &DecodeSettings::default()).unwrap();
    assert_eq!(header.progression_changes.len(), 1);
    assert_eq!(header.progression_changes[0].component_start, 256);
    assert_eq!(header.progression_changes[0].component_end, 16_384);

    let tiles = tile::parse(&mut reader, &header).unwrap();
    let tile = &tiles[0];
    let index =
        OwnershipIndex::new(&header.progression_changes, iter::once(tile), Cancel(None)).unwrap();
    let packets: Vec<_> = index
        .packets(tile, Cancel(None))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        packets,
        [ProgressionData {
            component: 256,
            ..Default::default()
        }]
    );
}

#[test]
fn large_component_resolution_index_stays_compact_and_skips_empty_resolutions() {
    use super::*;
    use crate::j2c::{codestream::ProgressionChange, tile};
    use crate::{cancel::Cancel, reader::BitReader};
    use ProgressionOrder::*;

    // 1x1 pixel at offset (1, 1) with 31 decompositions (32 resolutions):
    // resolutions 0..31 are empty (0 precincts) and only resolution 31 has 1 precinct.
    let mut siz = vec![0, 0];
    for value in [2_u32, 2, 1, 1, 2, 2, 1, 1] {
        siz.extend(value.to_be_bytes());
    }
    siz.extend(1_u16.to_be_bytes());
    siz.extend([7, 1, 1]);
    let levels = 31_u8;
    let mut cod = vec![1, 0, 0, 32, 0, levels, 4, 4, 0, 1];
    cod.extend(vec![0x11; usize::from(levels) + 1]);
    let mut qcd = vec![0x40];
    qcd.extend(vec![0x48; 3 * usize::from(levels) + 1]);
    let data = [0_u8; 4];
    let mut sot = vec![0, 0];
    sot.extend(u32::try_from(12 + 2 + data.len()).unwrap().to_be_bytes());
    sot.extend([0, 1]);

    let mut stream = vec![0xFF, 0x4F];
    stream.extend(marker(0xFF51, &siz));
    stream.extend(marker(0xFF52, &cod));
    stream.extend(marker(0xFF5C, &qcd));
    stream.extend(marker(0xFF90, &sot));
    stream.extend([0xFF, 0x93]);
    stream.extend(data);
    stream.extend([0xFF, 0xD9]);

    let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
    let mut tiles = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    let tile = &mut tiles[0];
    tile.component_infos
        .resize(16_384, tile.component_infos[0].clone());

    // 32 broad entries across 16,384 components x 32 resolutions x 32 layers.
    let changes: Vec<_> = (0_u16..32)
        .map(|i| ProgressionChange {
            resolution_start: 0,
            component_start: 0,
            layer_end: i + 1,
            resolution_end: 32,
            component_end: 16_384,
            progression_order: if i % 2 == 0 {
                LayerResolutionComponentPosition
            } else {
                ResolutionLayerComponentPosition
            },
        })
        .collect();

    let index = OwnershipIndex::new(&changes, iter::once(&*tile), Cancel(None)).unwrap();
    assert_eq!(index.run_count(), 32);
    assert!(
        index.estimated_bytes() < 8 * 1024,
        "expected compact index, got {} bytes",
        index.estimated_bytes()
    );

    // Only resolution 31 is non-empty, so first 3 packets come from layer 0, res 31, comps 0..3.
    let first_three: Vec<_> = index
        .packets(tile, Cancel(None))
        .unwrap()
        .map(Result::unwrap)
        .take(3)
        .collect();
    assert_eq!(
        first_three,
        [
            ProgressionData {
                layer_num: 0,
                resolution: 31,
                component: 0,
                precinct: 0,
            },
            ProgressionData {
                layer_num: 0,
                resolution: 31,
                component: 1,
                precinct: 0,
            },
            ProgressionData {
                layer_num: 0,
                resolution: 31,
                component: 2,
                precinct: 0,
            },
        ]
    );

    // Staggered bounds across 16,384 components x 32 resolutions x 32 entries also stay compact.
    let staggered: Vec<_> = (0_u16..32)
        .map(|i| ProgressionChange {
            resolution_start: (i % 16) as u8,
            component_start: i * 256,
            layer_end: i + 1,
            resolution_end: 32,
            component_end: 16_384,
            progression_order: LayerResolutionComponentPosition,
        })
        .collect();
    let staggered_index =
        OwnershipIndex::new(&staggered, iter::once(&*tile), Cancel(None)).unwrap();
    assert!(
        staggered_index.estimated_bytes() < 256 * 1024,
        "expected compact staggered index, got {} bytes",
        staggered_index.estimated_bytes()
    );

    let mut partial = staggered.clone();
    for (i, p) in partial.iter_mut().enumerate() {
        p.resolution_end = 31;
        p.component_end = if i % 2 == 0 { 12_288 } else { 16_384 };
    }
    partial.push(changes[0]);
    let partial_index = OwnershipIndex::new(&partial, iter::once(&*tile), Cancel(None)).unwrap();
    assert_eq!(partial_index.run_count(), 1);
    assert!(
        partial_index
            .packets(tile, Cancel(None))
            .unwrap()
            .map(Result::unwrap)
            .eq((0..16_384).map(|c| ProgressionData {
                resolution: 31,
                component: c,
                ..Default::default()
            }))
    );

    // A positional entry covering empty resolution 0 with overflowing stride still fails validation.
    let bad_changes = [ProgressionChange {
        resolution_start: 0,
        component_start: 0,
        layer_end: 1,
        resolution_end: 32,
        component_end: 16_384,
        progression_order: ResolutionPositionComponentLayer,
    }];
    let bad_index = OwnershipIndex::new(&bad_changes, iter::once(&*tile), Cancel(None)).unwrap();
    assert!(matches!(
        bad_index.packets(tile, Cancel(None)),
        Err(crate::DecodeError::Decoding(
            DecodingError::InvalidProgressionIterator
        ))
    ));
}

#[test]
fn shadowed_entries_produce_identical_packets_and_compact_index() {
    use super::*;
    use crate::j2c::{codestream::ProgressionChange, tile};
    use crate::{cancel::Cancel, reader::BitReader};
    use ProgressionOrder::*;

    let stream = multi_tile_stream();
    let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
    let tiles = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    let tile = &tiles[0];

    let build_changes = |shadowed_count: usize| {
        let mut changes = vec![
            ProgressionChange {
                resolution_start: 0,
                component_start: 0,
                layer_end: 6,
                resolution_end: 4,
                component_end: 2,
                progression_order: ResolutionPositionComponentLayer,
            },
            ProgressionChange {
                resolution_start: 0,
                component_start: 2,
                layer_end: 6,
                resolution_end: 4,
                component_end: 3,
                progression_order: ComponentPositionResolutionLayer,
            },
        ];
        // Append shadowed entries with varied bounds and orders (not caught solely by full-domain check).
        for i in 0..shadowed_count {
            changes.push(ProgressionChange {
                resolution_start: (i % 4) as u8,
                component_start: (i % 3) as u16,
                layer_end: ((i % 6) + 1) as u16,
                resolution_end: 4,
                component_end: 3,
                progression_order: match i % 5 {
                    0 => LayerResolutionComponentPosition,
                    1 => ResolutionLayerComponentPosition,
                    2 => ResolutionPositionComponentLayer,
                    3 => PositionComponentResolutionLayer,
                    _ => ComponentPositionResolutionLayer,
                },
            });
        }
        changes
    };

    let base_index =
        OwnershipIndex::new(&build_changes(0), iter::once(tile), Cancel(None)).unwrap();
    let small_index =
        OwnershipIndex::new(&build_changes(100), iter::once(tile), Cancel(None)).unwrap();
    let large_index =
        OwnershipIndex::new(&build_changes(4_000), iter::once(tile), Cancel(None)).unwrap();

    assert_eq!(small_index.run_count(), large_index.run_count());

    let base_packets: Vec<_> = base_index
        .packets(tile, Cancel(None))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let small_packets: Vec<_> = small_index
        .packets(tile, Cancel(None))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    let large_packets: Vec<_> = large_index
        .packets(tile, Cancel(None))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(base_packets, small_packets);
    assert_eq!(base_packets, large_packets);

    let mut big_tile = tile.clone();
    let template = &mut big_tile.component_infos[0].coding_style.parameters;
    template.num_resolution_levels = 32;
    template.num_decomposition_levels = 31;
    template.precinct_exponents.resize(32, (3, 3));
    big_tile.num_layers = 4;
    big_tile
        .component_infos
        .resize(16_384, big_tile.component_infos[0].clone());

    let build_big = |shadowed: usize| {
        let mut changes = vec![
            ProgressionChange {
                resolution_start: 0,
                component_start: 0,
                layer_end: 4,
                resolution_end: 32,
                component_end: 8_192,
                progression_order: LayerResolutionComponentPosition,
            },
            ProgressionChange {
                resolution_start: 0,
                component_start: 8_192,
                layer_end: 4,
                resolution_end: 32,
                component_end: 16_384,
                progression_order: ResolutionLayerComponentPosition,
            },
        ];
        for i in 0..shadowed {
            changes.push(ProgressionChange {
                resolution_start: (i % 32) as u8,
                component_start: ((i * 7) % 16_000) as u16,
                layer_end: ((i % 4) + 1) as u16,
                resolution_end: 32,
                component_end: 16_384,
                progression_order: LayerResolutionComponentPosition,
            });
        }
        changes
    };

    let idx_100 =
        OwnershipIndex::new(&build_big(100), iter::once(&big_tile), Cancel(None)).unwrap();
    let idx_4000 =
        OwnershipIndex::new(&build_big(4_000), iter::once(&big_tile), Cancel(None)).unwrap();
    assert_eq!(idx_100.run_count(), idx_4000.run_count());
    assert!(idx_4000.estimated_bytes() < 16 * 1024);
}

#[test]
fn ownership_index_respects_cancellation() {
    use super::*;
    use crate::j2c::{codestream::ProgressionChange, tile};
    use crate::{CancelHandle, DecodeError, cancel::Cancel, reader::BitReader};
    use ProgressionOrder::*;

    let stream = position_order_stream(1, 1, 4, &[]);
    let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
    let tiles = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    let tile = &tiles[0];
    let changes = [ProgressionChange {
        resolution_start: 0,
        component_start: 0,
        layer_end: 1,
        resolution_end: 2,
        component_end: 1,
        progression_order: ResolutionPositionComponentLayer,
    }];

    let cancelled = CancelHandle::new(|| true);
    assert!(matches!(
        OwnershipIndex::new(&changes, iter::once(tile), Cancel(Some(&cancelled))),
        Err(DecodeError::Cancelled)
    ));

    let index = OwnershipIndex::new(&changes, iter::once(tile), Cancel(None)).unwrap();
    assert!(matches!(
        index.packets(tile, Cancel(Some(&cancelled))),
        Err(DecodeError::Cancelled)
    ));
}

#[test]
fn edge_precincts_decode_one_sample() {
    for (origin, exponent) in [(u32::MAX - 1, 1), (u32::MAX - 3, 2)] {
        for (x, y) in [(origin, 1), (1, origin)] {
            for levels in [0, 2] {
                let data = position_order_stream(1, levels, 1, &[]);
                let mut image = Image::new(&data, &DecodeSettings::default()).unwrap();
                let size = &mut image.header.size_data;
                size.reference_grid_width = x + 1;
                size.reference_grid_height = y + 1;
                size.image_area_x_offset = x;
                size.image_area_y_offset = y;
                size.tile_x_offset = x;
                size.tile_y_offset = y;
                image.header.component_infos[0]
                    .coding_style
                    .parameters
                    .precinct_exponents
                    .fill((exponent, exponent));
                let mut ctx = DecoderContext::default();
                let decoded = image.decode(&mut ctx).unwrap();
                assert_eq!(decoded.data_u8().unwrap(), [128]);
            }
        }
    }
}

#[test]
fn subsampled_edge_tile_stores_the_final_sample() {
    let origin = u32::MAX - 1;
    for (x, y, sx, sy) in [(origin, 0, 2, 1), (0, origin, 1, 2)] {
        let mut siz = vec![0, 0];
        for value in [x + 1, y + 1, x, y, 1, 1, x, y] {
            siz.extend(value.to_be_bytes());
        }
        siz.extend([0, 1, 7, sx, sy]);
        let mut stream = vec![0xff, 0x4f];
        stream.extend(marker(0xff51, &siz));
        stream.extend(marker(0xff52, &[0, 0, 0, 1, 0, 0, 4, 4, 0, 1]));
        stream.extend(marker(0xff5c, &[0x40, 0x40]));
        stream.extend(marker(0xff90, &[0, 0, 0, 0, 0, 19, 0, 1]));
        stream.extend([0xff, 0x93, 0xcf, 0xb4, 0x08, 0x04, 0x5f, 0xff, 0xd9]);

        let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
        let mut ctx = DecoderContext::default();
        assert_eq!(image.decode(&mut ctx).unwrap().data_u8().unwrap(), [201]);
    }
}

#[test]
fn positional_coordinates_at_the_grid_edge() {
    use super::*;
    use crate::j2c::{rect::IntRect, tile};
    use crate::reader::BitReader;
    for (origin, levels, count) in [(u32::MAX - 3, 0, 1), (u32::MAX - 6, 4, 0)] {
        let stream = position_order_stream(1, levels, 1, &[]);
        let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
        let mut tiles = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
        let tile = &mut tiles[0];
        for (x, y, sx, sy) in [(origin, 0, 3, 1), (0, origin, 1, 3)] {
            tile.rect = IntRect::from_xywh(x, y, 1, 1);
            tile.component_infos[0].size_info.horizontal_resolution = sx;
            tile.component_infos[0].size_info.vertical_resolution = sy;
            let mut precincts =
                ResolutionTile::new(ComponentTile::new(tile, &tile.component_infos[0]), 0)
                    .precincts()
                    .unwrap()
                    .map(|p| (p.r_x, p.r_y))
                    .collect::<Vec<_>>();
            assert_eq!(precincts.len(), count);
            if count == 1 {
                assert_eq!(precincts.pop(), Some((x, y)));
            }
        }
    }
}

#[test]
fn inherited_entries_skip_deeper_resolutions() {
    use super::*;
    use crate::j2c::{codestream::ProgressionChange, tile};
    use crate::reader::BitReader;
    let stream = position_order_stream(1, 1, 2, &[]);
    let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
    let mut tiles = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    let deep = &mut tiles[0];
    deep.component_infos
        .resize(1024, deep.component_infos[0].clone());
    let mut shallow = deep.clone();
    for c in &mut shallow.component_infos {
        c.coding_style.parameters.num_resolution_levels = 1;
        c.coding_style.parameters.num_decomposition_levels = 0;
    }
    let change = ProgressionChange {
        resolution_start: 1,
        resolution_end: 2,
        component_start: 0,
        component_end: 1024,
        layer_end: 1,
        progression_order: ProgressionOrder::LayerResolutionComponentPosition,
    };
    let mut changes: Vec<_> = (0..1024)
        .map(|c| ProgressionChange {
            component_start: c,
            component_end: c + 1,
            ..change
        })
        .collect();
    changes.push(ProgressionChange {
        resolution_start: 0,
        resolution_end: 1,
        ..change
    });
    let index = OwnershipIndex::new(
        &changes,
        [&*deep, &shallow, &shallow].into_iter(),
        Cancel(None),
    )
    .unwrap();
    assert_eq!(index.cached((1, 1)).unwrap(), [1024]);
    let packets = index
        .packets(&shallow, Cancel(None))
        .unwrap()
        .map(Result::unwrap);
    assert!(
        packets.eq(progression_order_iterator(
            IteratorInput::new(&shallow),
            change.progression_order,
            Cancel(None)
        )
        .unwrap())
    );

    let shallow_info = shallow.component_infos[0].clone();
    deep.component_infos.clear();
    deep.component_infos.resize(14_336, shallow_info);
    let c = 8192;
    let style = &mut deep.component_infos[c].coding_style.parameters;
    style.num_resolution_levels = 32;
    style.num_decomposition_levels = 31;
    style.precinct_exponents.resize(32, (1, 1));
    let changes: Vec<_> = (0..112)
        .map(|i| ProgressionChange {
            component_start: i * 128,
            component_end: (i + 1) * 128,
            resolution_end: 32,
            ..change
        })
        .collect();
    let index = OwnershipIndex::new(&changes, iter::once(&*deep), Cancel(None)).unwrap();
    assert_eq!(index.run_count(), 31);
    assert!(index.estimated_bytes() < 1024);
    assert!(
        index
            .packets(deep, Cancel(None))
            .unwrap()
            .map(Result::unwrap)
            .eq((1..32).map(|r| ProgressionData {
                component: c as u16,
                resolution: r,
                ..Default::default()
            }))
    );
}

#[test]
fn singleton_shapes_share_only_the_ownership_runs() {
    use super::*;
    use crate::j2c::{codestream::ProgressionChange, tile};
    use crate::reader::BitReader;

    let data = position_order_stream(1, 31, 1, &[]);
    let image = Image::new(&data, &DecodeSettings::default()).unwrap();
    let base = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    let mut tiles = Vec::new();
    for depth in 1..=32 {
        for layers in 1..=32 {
            let mut tile = base[0].clone();
            tile.rect = crate::j2c::rect::IntRect::from_xywh(1, 0, 1, 1);
            tile.num_layers = layers;
            let style = &mut tile.component_infos[0].coding_style.parameters;
            style.num_resolution_levels = depth;
            style.num_decomposition_levels = depth - 1;
            tiles.push(tile);
        }
    }
    let order = ProgressionOrder::LayerResolutionComponentPosition;
    let first = ProgressionChange {
        resolution_start: 0,
        resolution_end: 16,
        component_start: 0,
        component_end: 1,
        layer_end: 32,
        progression_order: order,
    };
    let changes = [
        first,
        ProgressionChange {
            resolution_start: 16,
            resolution_end: 32,
            ..first
        },
    ];
    let index = OwnershipIndex::new(&changes, tiles.iter(), Cancel(None)).unwrap();
    assert!(index.estimated_bytes() < 1024);
    for tile in &tiles {
        let depth = tile.component_infos[0].num_resolution_levels();
        assert!(
            index
                .cached((1 << (depth - 1), u16::from(tile.num_layers)))
                .is_none()
        );
        let expected =
            progression_order_iterator(IteratorInput::new(tile), order, Cancel(None)).unwrap();
        assert!(
            index
                .packets(tile, Cancel(None))
                .unwrap()
                .map(Result::unwrap)
                .eq(expected)
        );
    }
}

#[test]
fn cached_shapes_match_resolution_and_layers_in_the_same_run() {
    use super::*;
    use crate::j2c::{codestream::ProgressionChange, rect::IntRect, tile};
    use crate::reader::BitReader;
    let data = position_order_stream(1, 1, 1, &[]);
    let image = Image::new(&data, &DecodeSettings::default()).unwrap();
    let mut tiles = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    let full = &mut tiles[0];
    full.num_layers = 2;
    let mut odd = full.clone();
    odd.rect = IntRect::from_xywh(1, 0, 1, 1);
    let mut shallow = full.clone();
    shallow.num_layers = 1;
    let style = &mut shallow.component_infos[0].coding_style.parameters;
    style.num_resolution_levels = 1;
    style.num_decomposition_levels = 0;
    let order = ProgressionOrder::ResolutionLayerComponentPosition;
    let first = ProgressionChange {
        resolution_start: 0,
        resolution_end: 1,
        component_start: 0,
        component_end: 1,
        layer_end: 1,
        progression_order: order,
    };
    let changes = [
        first,
        ProgressionChange {
            resolution_end: 2,
            layer_end: 2,
            ..first
        },
    ];
    let index = OwnershipIndex::new(
        &changes,
        [&*full, &odd, &odd, &shallow, &shallow].into_iter(),
        Cancel(None),
    )
    .unwrap();
    assert_eq!(index.cached((2, 2)).unwrap(), [1]);
    assert_eq!(index.cached((1, 1)).unwrap(), [0]);
    for tile in [&*full, &odd, &shallow] {
        let expected =
            progression_order_iterator(IteratorInput::new(tile), order, Cancel(None)).unwrap();
        assert!(
            index
                .packets(tile, Cancel(None))
                .unwrap()
                .map(Result::unwrap)
                .eq(expected)
        );
    }
}

#[test]
fn parser_cancels_during_progression_pull() {
    use super::*;
    use crate::j2c::{
        build, codestream::ProgressionChange, decode::DecompositionStorage, rect::IntRect, segment,
        tile,
    };
    use crate::{CancelHandle, DecodeError, reader::BitReader};
    use ProgressionOrder::*;
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};
    let stream = position_order_stream(1, 1, 1, &[]);
    let image = Image::new(&stream, &DecodeSettings::default()).unwrap();
    let base = tile::parse(&mut BitReader::new(image.codestream), &image.header).unwrap();
    for order in [
        LayerResolutionComponentPosition,
        ResolutionLayerComponentPosition,
        ResolutionPositionComponentLayer,
    ] {
        let mut full = base[0].clone();
        let positional = matches!(order, ResolutionPositionComponentLayer);
        let mut tile;
        if positional {
            full.rect = IntRect::from_xywh(0, 0, 2050, 1);
            tile = full.clone();
        } else {
            full.component_infos
                .resize(1025, full.component_infos[0].clone());
            tile = full.clone();
            for info in &mut tile.component_infos[..1024] {
                info.coding_style.parameters.num_resolution_levels = 1;
                info.coding_style.parameters.num_decomposition_levels = 0;
            }
        }
        let changes = [ProgressionChange {
            resolution_start: 1,
            resolution_end: 2,
            component_start: 0,
            component_end: full.component_infos.len() as u16,
            layer_end: 1,
            progression_order: order,
        }];
        let index =
            OwnershipIndex::new(&changes, [&full, &tile].into_iter(), Cancel(None)).unwrap();
        let mut storage = DecompositionStorage::default();
        build::build(&tile, &mut storage, 0, Cancel(None)).unwrap();
        let budget = Arc::new(AtomicUsize::new(usize::MAX));
        let remaining = budget.clone();
        let handle = CancelHandle::new(move || remaining.fetch_sub(1, Ordering::Relaxed) == 0);
        let cancel = Cancel(Some(&handle));
        let packets = index
            .packets(&tile, cancel)
            .unwrap()
            .inspect(|p| assert!(matches!(p, Err(DecodeError::Cancelled))));
        // Allow the parser and entry checks, plus run, component and first heap check for RPCL.
        budget.store(if positional { 5 } else { 2 }, Ordering::Relaxed);
        assert_eq!(
            segment::parse(
                &tile,
                Box::new(packets),
                &image.header,
                &mut storage,
                cancel
            ),
            Err(DecodeError::Cancelled)
        );
    }
}

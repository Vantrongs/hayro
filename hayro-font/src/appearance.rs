use crate::{
    FontData, GlyphId, StandardFont,
    program::{FontProgram, ProgramKind},
};
use kurbo::BezPath;
use rustc_hash::FxHashMap;

/// A font used for glyph outlines and advances in generated appearances.
pub struct AppearanceFont {
    program: FontProgram,
    standard: StandardFont,
    cmap: Option<FxHashMap<char, GlyphId>>,
    widths: Option<FxHashMap<GlyphId, f32>>,
}

impl AppearanceFont {
    /// Open a resolved font and apply standard metrics when it substitutes for a standard face.
    pub fn new(
        data: FontData,
        index: u32,
        standard: StandardFont,
        stands_in: bool,
    ) -> Option<Self> {
        let program = FontProgram::new(data, index)?;
        let bare = program.kind() == ProgramKind::Cff;
        let mut cmap = bare.then(FxHashMap::default);
        let mut widths = (bare || stands_in).then(FxHashMap::default);
        if let Some(widths) = &mut widths {
            for (gid, name) in program.font().glyph_names() {
                let id = GlyphId::from_skrifa(gid);
                if let Some(w) = standard.get_width(&name) {
                    widths.insert(id, w / 1000.0);
                    if let Some(map) = &mut cmap
                        && let Some(c) = crate::generated::glyph_names::get(&name)
                            .and_then(|s| s.chars().next())
                            .or_else(|| crate::uni_component(&name).and_then(|s| s.chars().next()))
                            .or_else(|| crate::u_component(&name))
                    {
                        map.entry(c).or_insert(id);
                    }
                }
            }
        }
        Some(Self {
            program,
            standard,
            cmap,
            widths,
        })
    }

    /// The number of glyphs in the program.
    pub fn num_glyphs(&self) -> u32 {
        self.program.font().num_glyphs()
    }

    /// Resolve a character to its glyph and advance in em units.
    pub fn glyph(&self, c: char) -> Option<(GlyphId, f32)> {
        let id = match &self.cmap {
            Some(map) => *map.get(&c)?,
            None => self.program.cmap_glyph(c)?,
        };
        if id == GlyphId::NOTDEF {
            return None;
        }
        let width = match &self.widths {
            Some(widths) => widths
                .get(&id)
                .copied()
                .or_else(|| self.program.advance_width(id).map(|v| v.round() / 1000.0))?,
            None => self.program.advance_width(id)? / 1000.0,
        };
        Some((id, width))
    }

    /// Resolve a glyph name in a bare CFF font.
    pub fn named(&self, name: &str) -> Option<(GlyphId, f32)> {
        self.cmap.as_ref()?;
        Some((
            self.program.glyph_by_name(name)?,
            self.standard.get_width(name)? / 1000.0,
        ))
    }

    /// The glyph outline in a 1000-unit em.
    pub fn outline(&self, id: GlyphId) -> BezPath {
        self.program.outline(id)
    }
}

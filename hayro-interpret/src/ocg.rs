//! Optional content (ISO 32000-2, 8.11) read as `PDFium` reads it
//! (`core/fpdfapi/page/cpdf_occontext.cpp`), which the cases of
//! `hayro/tests/optional_content` check on its verdicts: where the standard leaves a
//! form open (a malformed expression, a group the catalog does not list, `/Configs`)
//! the document draws as that viewer draws it. Unlike it, expressions are evaluated
//! at any depth, an expression among its own operands is off there, and an entry of
//! the table leading to another's body is that object (`PDFium` reads nothing).

use core::cell::RefCell;
use hayro_syntax::hash;
use hayro_syntax::lost::Lost;
use hayro_syntax::object::dict::keys::{D, INTENT, OC, OCGS, OCPROPERTIES, OFF, ON, P, TYPE, VE};
use hayro_syntax::object::{Array, Dict, MaybeRef, Object};
use hayro_syntax::xref::{ContentKey, Shallow, XRef};
use std::rc::Rc;

/// A value with where it was read: in the document, whose values last as long as it
/// and are told apart by where they are written ([`ContentKey`]), or in a content
/// stream, whose values last only while it is drawn.
#[derive(Clone)]
pub(crate) enum Kept<D, C> {
    Document(D),
    Content(C),
}

/// What each value of the document comes to, with what finding it lost.
type Memo<'a, V> = RefCell<hash::Map<ContentKey<'a>, (V, Lost)>>;

type Raw<'a, 'b> = Kept<MaybeRef<Object<'a>>, MaybeRef<Object<'b>>>;

impl<'b, 'a: 'b> Kept<Dict<'a>, Dict<'b>> {
    /// Entry `key`, kept where the dictionary is.
    fn raw(&self, key: &[u8]) -> Option<Raw<'a, 'b>> {
        match self {
            Self::Document(dict) => dict.get_raw(key).map(Kept::Document),
            Self::Content(dict) => dict.get_raw(key).map(Kept::Content),
        }
    }

    fn dict(&self) -> Dict<'b> {
        match self {
            Self::Document(dict) => dict.clone(),
            Self::Content(dict) => dict.clone(),
        }
    }
}

impl<'b, 'a: 'b> Kept<Array<'a>, Array<'b>> {
    /// The items, kept where the array is.
    fn items(&self) -> Vec<Raw<'a, 'b>> {
        match self {
            Self::Document(array) => array.raw_iter().map(Kept::Document).collect(),
            Self::Content(array) => array.raw_iter().map(Kept::Content).collect(),
        }
    }
}

/// How a place reads a stream: as its dictionary, where `PDFium` asks for a
/// dictionary (`GetDictFor`, `GetDictAt`: `/OC`, the items of a list of groups,
/// `/Usage`, `/View`), or as neither a dictionary nor an array, where it asks for
/// the object (`GetDirectObjectFor`: an operand of `/VE`, `/OCGs` of a membership
/// dictionary).
#[derive(Clone, Copy)]
enum Slot {
    Dict,
    Object,
}

/// What an item or entry holds, a dictionary or array with where it is written
/// when in the document: the body an entry of the table leads to, which every entry
/// leading there reads, its bytes in place, or for a stream's dictionary its bytes,
/// which no list of groups names (`PDFium` compares the stream).
enum Found<'a, 'b> {
    Dict(Option<ContentKey<'a>>, Kept<Dict<'a>, Dict<'b>>),
    Array(Option<ContentKey<'a>>, Kept<Array<'a>, Array<'b>>),
    /// Neither.
    Other,
    /// A reference to nothing.
    Nothing,
}

/// A name or string `PDFium` compares an entry with, or any other value.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Word {
    View,
    All,
    Off,
    Ocg,
    AnyOn,
    AnyOff,
    AllOn,
    AllOff,
    Not,
    And,
    Or,
    Other,
}

impl Word {
    /// The word `object` is, as `PDFium` reads a name or string either way.
    fn of(object: &Object<'_>) -> Self {
        let bytes = match object {
            Object::Name(name) => name.as_ref(),
            Object::String(string) => string.as_bytes(),
            _ => return Self::Other,
        };
        match bytes {
            b"View" => Self::View,
            b"All" => Self::All,
            b"OFF" => Self::Off,
            b"OCG" => Self::Ocg,
            b"AnyOn" => Self::AnyOn,
            b"AnyOff" => Self::AnyOff,
            b"AllOn" => Self::AllOn,
            b"AllOff" => Self::AllOff,
            b"Not" => Self::Not,
            b"And" => Self::And,
            b"Or" => Self::Or,
            _ => Self::Other,
        }
    }

    fn takes_view(self) -> bool {
        matches!(self, Self::View | Self::All)
    }
}

/// The optional content of a document, one for the document
/// ([`crate::interpret::annotation::Fields`]): its configuration for viewing, and
/// what each value of the document it reads comes to, found once for the document
/// by where it is written ([`Found`]), in a map of its own for each way it is read.
/// Each is kept with what finding it lost ([`Lost`]), which every later use is told
/// as the first was, as [`hayro_syntax::lost::Memo`] does. What a content stream
/// holds in place is found each time it is drawn.
///
/// A decision tells every object it depends on that cannot be read. What is read
/// whole and kept for many decisions (the configuration, a list of groups, the
/// operands of an expression) tells each decision that uses it everything reading
/// it lost, also an object `PDFium` would not reach for that decision: a copy that
/// refuses a lost read then refuses some damaged files it could copy, and never
/// copies one whose decision rested on a lost object. Which lost object is told
/// first may differ from the one `PDFium` meets first.
pub(crate) struct OptionalContent<'a> {
    xref: &'a XRef,
    /// The groups the catalog's `/OCProperties` lists, `None` where it lists none,
    /// with what reading them lost, which each group tells.
    listed: (Option<hash::Set<ContentKey<'a>>>, Lost),
    /// The configuration, with what reading it lost, which each listed group tells:
    /// one the catalog does not list is on before it is read.
    config: (Option<Config<'a>>, Lost),
    /// Whether each optional content an `/OC` names is on.
    actives: Memo<'a, bool>,
    /// Whether each group is on.
    groups: Memo<'a, bool>,
    /// What each value an entry refers to is as a word.
    words: Memo<'a, Word>,
    /// Whether each value of `/Intent` an entry refers to takes viewing, `None` for
    /// nothing there.
    intents: Memo<'a, Option<bool>>,
    /// The `/ViewState` of the `/View` of each `/Usage` an entry refers to.
    usages: Memo<'a, Option<bool>>,
    /// Each `/View` an entry refers to: `None` where it is no dictionary, else its
    /// `/ViewState`, if any.
    views: Memo<'a, Option<Option<bool>>>,
    expressions: RefCell<hash::Map<ContentKey<'a>, Evaluated>>,
    lists: Memo<'a, Listed>,
    /// The membership dictionaries, expressions and lists evaluated.
    #[cfg(test)]
    pub(crate) evaluated: core::cell::Cell<usize>,
    /// The groups, the values of `/Intent`, the words, and the groups of usage
    /// applications read.
    #[cfg(test)]
    pub(crate) read: core::cell::Cell<usize>,
}

/// The configuration that sets the groups the catalog lists, read once: the first
/// of `/Configs` whose `/Intent` takes viewing, else `/D`. Each group is known by
/// where it is written ([`Found`]).
struct Config<'a> {
    on: bool,
    turned_on: hash::Set<ContentKey<'a>>,
    turned_off: hash::Set<ContentKey<'a>>,
    /// The `/ViewState` usage applications for viewing (`/AS`) set, the last
    /// applying to a group.
    applied: hash::Map<ContentKey<'a>, bool>,
}

/// A visibility expression evaluated or being evaluated.
#[derive(Clone, Copy)]
enum Evaluated {
    /// Being evaluated, at this place in the order expressions were reached.
    Now(usize),
    /// Whether it is on, and what finding that lost.
    Done(bool, Lost),
}

/// How many of the groups a membership dictionary's `/OCGs` lists are on and off,
/// which decides each of its policies.
#[derive(Clone, Copy, Default)]
struct Listed {
    on: usize,
    off: usize,
}

/// The dictionary `object` is where a dictionary is asked for: a stream's.
fn as_dict<'o>(object: Object<'o>) -> Option<Dict<'o>> {
    match object {
        Object::Dict(dict) => Some(dict),
        Object::Stream(stream) => Some(stream.dict().clone()),
        _ => None,
    }
}

impl<'a> Config<'a> {
    /// Whether `group`, which the catalog lists, is on as the configuration sets it.
    fn state(&self, group: ContentKey<'a>) -> bool {
        let mut on = self.on;
        if self.turned_on.contains(&group) {
            on = true;
        }
        if self.turned_off.contains(&group) {
            on = false;
        }
        self.applied.get(&group).copied().unwrap_or(on)
    }
}

impl<'a> OptionalContent<'a> {
    /// The optional content of the document `xref` reads.
    pub(crate) fn new(xref: &'a XRef) -> Self {
        let mut content = Self {
            xref,
            listed: (None, None),
            config: (None, None),
            actives: RefCell::default(),
            groups: RefCell::default(),
            words: RefCell::default(),
            intents: RefCell::default(),
            usages: RefCell::default(),
            views: RefCell::default(),
            expressions: RefCell::default(),
            lists: RefCell::default(),
            #[cfg(test)]
            evaluated: core::cell::Cell::default(),
            #[cfg(test)]
            read: core::cell::Cell::default(),
        };
        let (properties, lost) = xref.apart(|| {
            let catalog = xref.get::<Dict<'a>>(xref.root_id())?;
            let properties = catalog.get::<Dict<'a>>(OCPROPERTIES)?;
            let listed = content
                .identities(&properties.get::<Array<'a>>(OCGS)?)
                .collect();
            Some((properties, listed))
        });
        let (properties, listed) = properties.unzip();
        content.listed = (listed, lost);
        content.config = xref.apart(|| content.configuration(&properties?));
        content
    }

    /// Whether this is the optional content of the document `xref` reads.
    pub(crate) fn of(&self, xref: &XRef) -> bool {
        core::ptr::eq(self.xref, xref)
    }

    #[cfg(test)]
    fn count(&self) {
        self.evaluated.set(self.evaluated.get() + 1);
    }

    #[cfg(not(test))]
    fn count(&self) {}

    #[cfg(test)]
    fn count_read(&self) {
        self.read.set(self.read.get() + 1);
    }

    #[cfg(not(test))]
    fn count_read(&self) {}

    /// The groups `array` of the document lists, by reference or in place, without
    /// reading what a reference leads to.
    fn identities<'r>(&'r self, array: &'r Array<'a>) -> impl Iterator<Item = ContentKey<'a>> + 'r {
        array.raw_iter().filter_map(|item| match item {
            MaybeRef::Ref(r) => self.xref.content_key(r.into()),
            MaybeRef::NotRef(Object::Dict(group)) => Some(group.content_key()),
            MaybeRef::NotRef(_) => None,
        })
    }

    /// The dictionary `item` of the document holds where a dictionary is asked for,
    /// once for every item leading to one body: `None` for one read before, as for
    /// what is no dictionary.
    fn first_dict(
        &self,
        item: MaybeRef<Object<'a>>,
        seen: &mut hash::Set<ContentKey<'a>>,
    ) -> Option<Dict<'a>> {
        match item {
            MaybeRef::Ref(r) => {
                if !seen.insert(self.xref.content_key(r.into())?) {
                    return None;
                }
                as_dict(self.xref.get_shallow(r.into())?.into_object())
            }
            MaybeRef::NotRef(object) => as_dict(object),
        }
    }

    /// The configuration of the catalog's `/OCProperties`, each value an entry
    /// refers to read once however many entries refer to it, and `/AS` from its end,
    /// each list of groups applied once, where its last application decides.
    fn configuration(&self, properties: &Dict<'a>) -> Option<Config<'a>> {
        let mut seen = hash::Set::default();
        let viewing = properties.get::<Array<'a>>(b"Configs").and_then(|configs| {
            configs.raw_iter().find_map(|item| {
                let config = self.first_dict(item, &mut seen)?;
                // An `/Intent` that refers to nothing takes nothing.
                let intent = config.get_raw::<Object<'a>>(INTENT)?;
                self.takes_view(intent).unwrap_or(false).then_some(config)
            })
        });
        let config = viewing.or_else(|| properties.get::<Dict<'a>>(D))?;
        let mut applied = hash::Map::default();
        let usages: Vec<_> = config
            .get::<Array<'a>>(b"AS")
            .iter()
            .flat_map(|a| a.raw_iter())
            .collect();
        let (mut seen_usages, mut seen_lists) = (hash::Set::default(), hash::Set::default());
        for item in usages.into_iter().rev() {
            let Some(usage) = self.first_dict(item, &mut seen_usages) else {
                continue;
            };
            if !self.entry(&usage, b"Event").is_none_or(|e| e == Word::View) {
                continue;
            }
            let Some(Some(state)) = usage.get_raw::<Object<'a>>(b"View").map(|v| self.view(v))
            else {
                continue;
            };
            let groups = match usage.get_raw::<Object<'a>>(OCGS) {
                Some(MaybeRef::Ref(r)) => {
                    match self.xref.content_key(r.into()) {
                        Some(body) if seen_lists.insert(body) => {}
                        _ => continue,
                    }
                    match self.xref.get::<Array<'a>>(r.into()) {
                        Some(groups) => groups,
                        None => continue,
                    }
                }
                Some(MaybeRef::NotRef(Object::Array(groups))) => groups,
                _ => continue,
            };
            let on = state != Some(false);
            for group in self.identities(&groups) {
                self.count_read();
                applied.entry(group).or_insert(on);
            }
        }
        let named = |key: &[u8]| -> hash::Set<ContentKey<'a>> {
            config
                .get::<Array<'a>>(key)
                .iter()
                .flat_map(|a| self.identities(a).collect::<Vec<_>>())
                .collect()
        };
        Some(Config {
            on: self.entry(&config, b"BaseState") != Some(Word::Off),
            turned_on: named(ON),
            turned_off: named(OFF),
            applied,
        })
    }

    /// `compute` for `key` the first time, kept in `map` with what it lost, which
    /// every ask is told.
    fn once<V: Copy>(
        &self,
        map: &Memo<'a, V>,
        key: ContentKey<'a>,
        compute: impl FnOnce() -> V,
    ) -> V {
        let known = map.borrow().get(&key).copied();
        let (value, lost) = known.unwrap_or_else(|| {
            let made = self.xref.watch(compute);
            map.borrow_mut().insert(key, made);
            made
        });
        self.xref.note(lost);
        value
    }

    /// [`Self::once`] where `key` says where the value is written, each time where
    /// it is not in the document.
    fn kept<V: Copy>(
        &self,
        map: &Memo<'a, V>,
        key: Option<ContentKey<'a>>,
        compute: impl FnOnce() -> V,
    ) -> V {
        match key {
            Some(key) => self.once(map, key, compute),
            None => compute(),
        }
    }

    /// `compute` of what `raw` is: once for what a reference leads to, by its body
    /// in `map`; for a value in place, each time.
    fn referred<V: Copy>(
        &self,
        map: &Memo<'a, V>,
        raw: MaybeRef<Object<'_>>,
        compute: impl Fn(Option<Object<'_>>) -> V,
    ) -> V {
        match raw {
            MaybeRef::Ref(r) => match self.xref.content_key(r.into()) {
                Some(key) => self.once(map, key, || {
                    compute(self.xref.get_shallow(r.into()).map(Shallow::into_object))
                }),
                None => compute(None),
            },
            MaybeRef::NotRef(value) => compute(Some(value)),
        }
    }

    /// The word `raw` is, as `PDFium` reads a byte string: a reference to nothing
    /// is none.
    fn word(&self, raw: MaybeRef<Object<'_>>) -> Word {
        self.referred(&self.words, raw, |value| {
            self.count_read();
            value.map_or(Word::Other, |v| Word::of(&v))
        })
    }

    /// Entry `key` of `dict` as `PDFium` reads a byte string
    /// (`CPDF_Dictionary::GetByteStringFor`): `None` without the entry.
    fn entry(&self, dict: &Dict<'_>, key: &[u8]) -> Option<Word> {
        dict.get_raw::<Object<'_>>(key).map(|raw| self.word(raw))
    }

    /// Whether the value of `/Intent` `raw` takes viewing: `/View` or `/All`, or an
    /// array holding either; `None` where it refers to nothing.
    fn takes_view(&self, raw: MaybeRef<Object<'_>>) -> Option<bool> {
        self.referred(&self.intents, raw, |intent| {
            self.count_read();
            Some(match intent? {
                Object::Array(intents) => intents.raw_iter().any(|i| self.word(i).takes_view()),
                other => Word::of(&other).takes_view(),
            })
        })
    }

    /// The `/ViewState` of usage dictionary's `/View` `raw`, `None` where it is no
    /// dictionary.
    fn view(&self, raw: MaybeRef<Object<'_>>) -> Option<Option<bool>> {
        self.referred(&self.views, raw, |view| {
            let view = as_dict(view?)?;
            Some(self.entry(&view, b"ViewState").map(|v| v != Word::Off))
        })
    }

    /// What `item` holds, read where it is kept as `slot` reads it, a reference
    /// from the document.
    fn found<'b>(&self, item: Raw<'a, 'b>, slot: Slot) -> Found<'a, 'b>
    where
        'a: 'b,
    {
        match item {
            Kept::Document(MaybeRef::Ref(r)) | Kept::Content(MaybeRef::Ref(r)) => {
                let body = self.xref.content_key(r.into());
                match self.xref.get_shallow(r.into()) {
                    Some(Shallow::Value(Object::Dict(dict))) => {
                        Found::Dict(body, Kept::Document(dict))
                    }
                    Some(Shallow::Value(Object::Array(array))) => {
                        Found::Array(body, Kept::Document(array))
                    }
                    Some(Shallow::Stream(dict)) if matches!(slot, Slot::Dict) => {
                        Found::Dict(Some(dict.content_key()), Kept::Document(dict))
                    }
                    Some(_) => Found::Other,
                    None => Found::Nothing,
                }
            }
            Kept::Document(MaybeRef::NotRef(Object::Dict(dict))) => {
                Found::Dict(Some(dict.content_key()), Kept::Document(dict))
            }
            Kept::Document(MaybeRef::NotRef(Object::Array(array))) => {
                Found::Array(Some(array.content_key()), Kept::Document(array))
            }
            Kept::Content(MaybeRef::NotRef(Object::Dict(dict))) => {
                Found::Dict(None, Kept::Content(dict))
            }
            Kept::Content(MaybeRef::NotRef(Object::Array(array))) => {
                Found::Array(None, Kept::Content(array))
            }
            Kept::Document(MaybeRef::NotRef(_)) | Kept::Content(MaybeRef::NotRef(_)) => {
                Found::Other
            }
        }
    }

    /// What group `group`'s own entries decide (`CPDF_OCContext::LoadOCGState`): on
    /// when its `/Intent` does not take viewing, else its usage's `/ViewState`, if
    /// any.
    fn decided(&self, group: &Dict<'_>) -> Option<bool> {
        // An `/Intent` that refers to nothing is none.
        let intends = group
            .get_raw::<Object<'_>>(INTENT)
            .is_none_or(|raw| self.takes_view(raw).unwrap_or(true));
        if !intends {
            return Some(true);
        }
        let usage = group.get_raw::<Object<'_>>(b"Usage")?;
        self.referred(&self.usages, usage, |usage| {
            let view = as_dict(usage?)?.get_raw::<Object<'_>>(b"View")?;
            self.view(view).flatten()
        })
    }

    /// Whether group `group`, written at `key` when in the document, is on
    /// (`CPDF_OCContext::GetOCGVisible`): once for each place. One elsewhere is
    /// listed nowhere.
    fn group(&self, key: Option<ContentKey<'a>>, group: &Dict<'_>) -> bool {
        self.kept(&self.groups, key, || {
            self.count_read();
            self.decided(group).unwrap_or_else(|| {
                key.is_none_or(|key| {
                    let (listed, lost) = &self.listed;
                    self.xref.note(*lost);
                    if !listed.as_ref().is_some_and(|l| l.contains(&key)) {
                        return true;
                    }
                    let (config, lost) = &self.config;
                    self.xref.note(*lost);
                    config.as_ref().is_none_or(|c| c.state(key))
                })
            })
        })
    }

    /// Whether the optional content `oc`, written at `key` when in the document, is
    /// on (`CPDF_OCContext::CheckOCGDictVisible`): a group without `/Type` or with
    /// the type `/OCG`, else a membership dictionary; a `/Type` that refers to
    /// nothing is neither.
    fn active<'b>(&self, key: Option<ContentKey<'a>>, oc: &Kept<Dict<'a>, Dict<'b>>) -> bool
    where
        'a: 'b,
    {
        self.kept(&self.actives, key, || {
            let dict = oc.dict();
            if self.entry(&dict, TYPE).is_none_or(|t| t == Word::Ocg) {
                self.group(key, &dict)
            } else {
                self.membership(oc)
            }
        })
    }

    /// Whether the optional content entry `key` of `owner` names is on, `None` where
    /// it names no dictionary.
    pub(crate) fn names<'b>(&self, owner: &Kept<Dict<'a>, Dict<'b>>, key: &[u8]) -> Option<bool>
    where
        'a: 'b,
    {
        self.named(owner, key).map(|on| on())
    }

    /// [`Self::names`], whether it is on found only when asked: `PDFium` reads the
    /// dictionary where content begins (`CPDF_StreamContentParser`) and decides it
    /// only where the levels around it show (`CPDF_OCContext::CheckPageObjectVisible`).
    fn named<'b, 's>(
        &'s self,
        owner: &Kept<Dict<'a>, Dict<'b>>,
        key: &[u8],
    ) -> Option<impl FnOnce() -> bool + use<'a, 'b, 's>>
    where
        'a: 'b,
    {
        match self.found(owner.raw(key)?, Slot::Dict) {
            Found::Dict(at, oc) => Some(move || self.active(at, &oc)),
            _ => None,
        }
    }

    /// Whether membership dictionary `ocmd` is on (`CPDF_OCContext::LoadOCMDState`):
    /// by its visibility expression where `/VE` is an array, else by `/OCGs` and
    /// `/P`.
    fn membership<'b>(&self, ocmd: &Kept<Dict<'a>, Dict<'b>>) -> bool
    where
        'a: 'b,
    {
        self.count();
        if let Some(Found::Array(at, expression)) =
            ocmd.raw(VE).map(|e| self.found(e, Slot::Object))
        {
            return self.expression(at, expression);
        }
        let listed = match ocmd.raw(OCGS).map(|o| self.found(o, Slot::Object)) {
            Some(Found::Dict(at, group)) => return self.group(at, &group.dict()),
            Some(Found::Array(at, list)) => self.kept(&self.lists, at, || self.listed(&list)),
            None | Some(Found::Other | Found::Nothing) => return true,
        };
        if listed.on + listed.off == 0 {
            return true;
        }
        match self.entry(&ocmd.dict(), P).unwrap_or(Word::AnyOn) {
            Word::AnyOn => listed.on > 0,
            Word::AnyOff => listed.off > 0,
            Word::AllOn => listed.off == 0,
            Word::AllOff => listed.on == 0,
            _ => false,
        }
    }

    /// The groups `list` lists: each dictionary, by reference or in place.
    fn listed<'b>(&self, list: &Kept<Array<'a>, Array<'b>>) -> Listed
    where
        'a: 'b,
    {
        self.count();
        let mut listed = Listed::default();
        for item in list.items() {
            let on = match self.found(item, Slot::Dict) {
                Found::Dict(at, group) => self.group(at, &group.dict()),
                _ => continue,
            };
            if on {
                listed.on += 1;
            } else {
                listed.off += 1;
            }
        }
        listed
    }

    /// Whether visibility expression `expression` is on (`CPDF_OCContext::GetOCGVE`):
    /// `/And`, `/Or` or `/Not`, a name or string, followed by its operands, each a
    /// group or an expression. An unknown operator is off; `/Not` reads its first
    /// operand only, and is off without one or where it is neither; an operand of
    /// `/And` or `/Or` that is neither is off, one that refers to nothing is left
    /// out, the first operand read starting the value only when it is the first
    /// written.
    ///
    /// Evaluated with a stack of its own, however deeply expressions nest, each one
    /// of the document once: one is read where it is written, not through the
    /// expression that refers to it, so its cost does not grow with how deep it is.
    /// Expressions that are operands of each other, which `PDFium` evaluates to a
    /// depth of 32, are off where the cycle closes, and each is kept with what
    /// reading any of them lost.
    fn expression<'b>(
        &self,
        key: Option<ContentKey<'a>>,
        expression: Kept<Array<'a>, Array<'b>>,
    ) -> bool
    where
        'a: 'b,
    {
        #[derive(Clone, Copy)]
        enum Operator {
            Not,
            And,
            Or,
        }
        /// An expression being evaluated: where it is written, if in the document,
        /// its place in the order expressions were reached and the earliest place
        /// among those being evaluated that it or an operand leads back to, its
        /// operator, its operands left with their places among them, the place of
        /// the one being evaluated, its value so far, and what reading it and them
        /// lost.
        struct Frame<'a, 'b> {
            key: Option<ContentKey<'a>>,
            order: usize,
            low: usize,
            operator: Operator,
            operands: std::vec::IntoIter<(usize, Operand<'a, 'b>)>,
            pending: Option<usize>,
            value: bool,
            lost: Lost,
        }
        enum Operand<'a, 'b> {
            Group(Option<ContentKey<'a>>, Dict<'b>),
            Expression(Option<ContentKey<'a>>, Kept<Array<'a>, Array<'b>>),
            /// Neither a group nor an expression.
            Off,
            /// A reference to nothing.
            Absent,
        }
        /// An expression reached: known, with what finding it lost, being evaluated
        /// at a place, or to evaluate.
        enum Entered<'a, 'b> {
            Known(bool, Lost),
            Now(usize),
            Frame(Frame<'a, 'b>),
        }
        let operand = |item: Raw<'a, 'b>| -> Operand<'a, 'b> {
            match self.found(item, Slot::Object) {
                Found::Dict(at, group) => Operand::Group(at, group.dict()),
                Found::Array(at, nested) => Operand::Expression(at, nested),
                Found::Other => Operand::Off,
                Found::Nothing => Operand::Absent,
            }
        };
        let mut reached = 0;
        let mut frame = |key: Option<ContentKey<'a>>,
                         expression: &Kept<Array<'a>, Array<'b>>|
         -> Option<Frame<'a, 'b>> {
            self.count();
            let mut items = expression.items().into_iter();
            let operator = match items.next()? {
                Kept::Document(raw) => self.word(raw),
                Kept::Content(raw) => self.word(raw),
            };
            let operator = match operator {
                Word::Not => Operator::Not,
                Word::And => Operator::And,
                Word::Or => Operator::Or,
                _ => return None,
            };
            let taken = match operator {
                Operator::Not => 1,
                _ => usize::MAX,
            };
            reached += 1;
            Some(Frame {
                key,
                order: reached,
                low: reached,
                operator,
                operands: items
                    .take(taken)
                    .map(operand)
                    .enumerate()
                    .collect::<Vec<_>>()
                    .into_iter(),
                pending: None,
                value: false,
                lost: None,
            })
        };
        let mut enter =
            |key: Option<ContentKey<'a>>, nested: Kept<Array<'a>, Array<'b>>| -> Entered<'a, 'b> {
                if let Some(key) = key {
                    let known = self.expressions.borrow().get(&key).copied();
                    match known {
                        Some(Evaluated::Done(on, lost)) => {
                            self.xref.note(lost);
                            return Entered::Known(on, lost);
                        }
                        Some(Evaluated::Now(order)) => return Entered::Now(order),
                        None => {}
                    }
                }
                match self.xref.watch(|| frame(key, &nested)) {
                    (Some(frame), lost) => {
                        if let Some(key) = key {
                            self.expressions
                                .borrow_mut()
                                .insert(key, Evaluated::Now(frame.order));
                        }
                        Entered::Frame(Frame { lost, ..frame })
                    }
                    (None, lost) => {
                        if let Some(key) = key {
                            self.expressions
                                .borrow_mut()
                                .insert(key, Evaluated::Done(false, lost));
                        }
                        Entered::Known(false, lost)
                    }
                }
            };
        /// `frame` with the value of its operand at `at`, `None` for one left out.
        fn take(frame: &mut Frame<'_, '_>, at: usize, on: Option<bool>) {
            match (frame.operator, on) {
                (_, None) => {}
                (Operator::Not, Some(on)) => frame.value = !on,
                (_, Some(on)) if at == 0 => frame.value = on,
                (Operator::And, Some(on)) => frame.value = frame.value && on,
                (Operator::Or, Some(on)) => frame.value = frame.value || on,
            }
        }

        let mut stack = match enter(key, expression) {
            Entered::Known(on, _) => return on,
            Entered::Now(_) => return false,
            Entered::Frame(frame) => vec![frame],
        };
        // Expressions evaluated that lead back to one still being evaluated, kept
        // until it is done: with their values, kept then with what reading any of
        // them lost.
        let mut cycle: Vec<(usize, ContentKey<'a>, bool)> = Vec::new();
        loop {
            let top = stack.last_mut().expect("an expression being evaluated");
            match top.operands.next() {
                Some((at, Operand::Group(place, group))) => {
                    let (on, lost) = self.xref.watch(|| self.group(place, &group));
                    top.lost = top.lost.or(lost);
                    take(top, at, Some(on));
                }
                // `/Not` of an operand that is neither stays off.
                Some((at, Operand::Off)) => {
                    if !matches!(top.operator, Operator::Not) {
                        take(top, at, Some(false));
                    }
                }
                Some((at, Operand::Absent)) => take(top, at, None),
                Some((at, Operand::Expression(place, nested))) => match enter(place, nested) {
                    Entered::Known(on, lost) => {
                        take(top, at, Some(on));
                        top.lost = top.lost.or(lost);
                    }
                    Entered::Now(order) => {
                        top.low = top.low.min(order);
                        take(top, at, Some(false));
                    }
                    Entered::Frame(frame) => {
                        top.pending = Some(at);
                        stack.push(frame);
                    }
                },
                None => {
                    let done = stack.pop().expect("the top");
                    if done.low < done.order {
                        if let Some(key) = done.key {
                            cycle.push((done.order, key, done.value));
                        }
                    } else {
                        while let Some((_, key, value)) =
                            cycle.pop_if(|(order, _, _)| *order > done.order)
                        {
                            self.expressions
                                .borrow_mut()
                                .insert(key, Evaluated::Done(value, done.lost));
                        }
                        if let Some(key) = done.key {
                            self.expressions
                                .borrow_mut()
                                .insert(key, Evaluated::Done(done.value, done.lost));
                        }
                    }
                    match stack.last_mut() {
                        Some(parent) => {
                            parent.lost = parent.lost.or(done.lost);
                            parent.low = parent.low.min(done.low);
                            let at = parent.pending.take().expect("an operand evaluated");
                            take(parent, at, Some(done.value));
                        }
                        None => return done.value,
                    }
                }
            }
        }
    }

    /// Whether the optional content `/OC` of `owner`, an annotation, names is on.
    /// One without is on.
    pub(crate) fn shows(&self, owner: &Dict<'a>) -> bool {
        self.names(&Kept::Document(owner.clone()), OC)
            .unwrap_or(true)
    }
}

/// Which content shows as content and optional content begin and end: the
/// document's [`OptionalContent`] and the visibility of each level of content
/// begun, those below `floor` begun by the content streams that draw the one being
/// drawn, which its `EMC` does not end (`PDFium` parses each content stream with
/// marks of its own).
pub(crate) struct OcgState<'a> {
    content: Option<Rc<OptionalContent<'a>>>,
    visibility_stack: Vec<bool>,
    floor: usize,
}

impl<'a> OcgState<'a> {
    /// Without `content`, as `PDFium` draws soft masks and pattern cells, all content
    /// shows and no optional content is read.
    pub(crate) fn new(content: Option<Rc<OptionalContent<'a>>>) -> Self {
        Self {
            content,
            visibility_stack: Vec::new(),
            floor: 0,
        }
    }

    /// Begins drawing a content stream; [`Self::close`] with what this returns
    /// ends it, with every level it begun and left open.
    pub(crate) fn open(&mut self) -> usize {
        core::mem::replace(&mut self.floor, self.visibility_stack.len())
    }

    pub(crate) fn close(&mut self, floor: usize) {
        self.visibility_stack.truncate(self.floor);
        self.floor = floor;
    }

    /// Begins content in the optional content entry `key` of `owner` names, where it
    /// names one; `false`, beginning nothing, where it does not.
    pub(crate) fn begin_named<'b>(&mut self, owner: &Kept<Dict<'a>, Dict<'b>>, key: &[u8]) -> bool
    where
        'a: 'b,
    {
        let Some(content) = self.content.as_ref() else {
            return false;
        };
        let Some(on) = content.named(owner, key) else {
            return false;
        };
        let visible = self.is_visible() && on();
        self.visibility_stack.push(visible);
        true
    }

    /// What a content stream drawn for the one being drawn reads, as `PDFium` draws a
    /// Type 3 glyph with the options of the text showing it.
    pub(crate) fn content(&self) -> Option<Rc<OptionalContent<'a>>> {
        self.content.clone()
    }

    pub(crate) fn begin_marked_content(&mut self) {
        let visible = self.is_visible();
        self.visibility_stack.push(visible);
    }

    pub(crate) fn end_marked_content(&mut self) {
        if self.visibility_stack.len() > self.floor {
            self.visibility_stack.pop();
        }
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visibility_stack.last().copied().unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::OptionalContent;
    use crate::fixtures;
    use crate::interpret::annotation::Fields;
    use hayro_syntax::Pdf;
    use hayro_syntax::object::{Dict, ObjectIdentifier};
    use std::rc::Rc;

    /// A file whose group 4 is off, then `objects` from 5 on, then `aliases`.
    fn file(objects: impl IntoIterator<Item = String>, aliases: &[usize]) -> Pdf {
        let mut all = vec![
            b"<< /Type /Catalog /Pages 2 0 R \
              /OCProperties << /OCGs [4 0 R] /D << /OFF [4 0 R] >> >> >>"
                .to_vec(),
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec(),
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>".to_vec(),
            b"<< /Type /OCG /Name (Off) >>".to_vec(),
        ];
        all.extend(objects.into_iter().map(String::into_bytes));
        Pdf::new(fixtures::file_with_aliases(all, aliases)).expect("valid pdf")
    }

    /// Whether each of objects `owners` is shown, and the expressions and
    /// membership dictionaries evaluated to find out.
    fn shown(pdf: &Pdf, owners: impl IntoIterator<Item = u32>) -> (Vec<bool>, usize) {
        let state = OptionalContent::new(pdf.xref());
        let shown = owners
            .into_iter()
            .map(|n| {
                let owner = pdf
                    .xref()
                    .get::<Dict<'_>>(ObjectIdentifier::new(n as i32, 0))
                    .expect("an owner");
                state.shows(&owner)
            })
            .collect();
        (shown, state.evaluated.get())
    }

    /// An expression many owners share is evaluated once for all of them, each
    /// expression it refers to once.
    #[test]
    fn a_shared_expression_is_evaluated_once() {
        let (chain, owners) = (1_000, 1_000);
        // 5 … 5 + chain: [/And next] down to [/Not 4 0 R]; then the owners.
        let first_owner = 5 + chain + 1;
        let objects = (0..chain)
            .map(|i| format!("[/And {} 0 R]", 6 + i))
            .chain(["[/Not 4 0 R]".to_owned()])
            .chain((0..owners).map(|_| "<< /OC << /Type /OCMD /VE 5 0 R >> >>".to_owned()));
        let pdf = file(objects, &[]);
        let (shown, evaluated) = shown(&pdf, first_owner..first_owner + owners);
        assert!(shown.iter().all(|on| *on));
        assert_eq!(evaluated as u32, owners + chain + 1);
    }

    /// An expression that refers to the same expression twice, at every level,
    /// evaluates each once.
    #[test]
    fn a_forked_chain_is_evaluated_once_per_expression() {
        let depth = 20;
        let objects = (0..depth)
            .map(|i| format!("[/And {0} 0 R {0} 0 R]", 6 + i))
            .chain(["[/Not 4 0 R]".to_owned()])
            .chain(["<< /OC << /Type /OCMD /VE 5 0 R >> >>".to_owned()]);
        let pdf = file(objects, &[]);
        let (shown, evaluated) = shown(&pdf, [5 + depth + 1]);
        assert_eq!(shown, [true]);
        assert_eq!(evaluated as u32, 1 + depth + 1);
    }

    /// Entries of the table that lead to one expression's body evaluate it once,
    /// as do references to one membership dictionary.
    #[test]
    fn aliases_of_an_expression_are_evaluated_once() {
        let (operands, aliases) = (1_000, 1_000);
        let expression = format!("[/Or {}]", vec!["4 0 R"; operands].join(" "));
        // 5 is the expression, 6 a membership dictionary, 7 … owners, then the
        // entries leading to 5.
        let first_alias = 7 + 2 * aliases as u32;
        let objects = [expression, "<< /Type /OCMD /VE 5 0 R >>".to_owned()]
            .into_iter()
            .chain(
                (0..aliases as u32)
                    .map(|i| format!("<< /OC << /Type /OCMD /VE {} 0 R >> >>", first_alias + i)),
            )
            .chain((0..aliases).map(|_| "<< /OC 6 0 R >>".to_owned()));
        let pdf = file(objects, &vec![5; aliases]);
        let (shown, evaluated) = shown(&pdf, 7..first_alias);
        assert!(shown.iter().all(|on| !on));
        // A dictionary written in place per owner, the shared one once, and the
        // expression once.
        assert_eq!(evaluated, aliases + 1 + 1);
    }

    /// A membership dictionary written in place, with its expression and list,
    /// is evaluated once however often what holds it is drawn.
    #[test]
    fn a_membership_written_in_place_is_evaluated_once() {
        let operands = vec!["4 0 R"; 1_000].join(" ");
        let pdf = file(
            [format!(
                "<< /OC << /Type /OCMD /OCGs [{operands}] /VE [/And [/Or {operands}] 4 0 R] >> >>"
            )],
            &[],
        );
        let (shown, evaluated) = shown(&pdf, std::iter::repeat_n(5, 1_000));
        assert!(shown.iter().all(|on| !on));
        // The dictionary, and its expression of two.
        assert_eq!(evaluated, 1 + 2);
    }

    /// A group's body is read once for every entry leading to it, and an
    /// `/Intent` many groups refer to once for all of them.
    #[test]
    fn group_bodies_and_shared_intents_are_read_once() {
        let (groups, aliases, intents) = (500, 500, 1_000);
        let names = (0..intents)
            .map(|i| format!("/I{i}"))
            .collect::<Vec<_>>()
            .join(" ");
        // 5 is the intents; 6 … groups, then owners of each; then entries leading
        // to group 6, then their owners.
        let first_owner = 6 + groups;
        let first_alias = first_owner + groups + aliases;
        let off: Vec<String> = (6..6 + groups)
            .chain(first_alias..first_alias + aliases)
            .map(|n| format!("{n} 0 R"))
            .collect();
        let catalog = format!(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [{0}] \
             /D << /OFF [{0}] >> >> >>",
            off.join(" ")
        );
        let objects = [
            catalog,
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>".to_owned(),
            "<< /Type /OCG /Name (Unused) >>".to_owned(),
            format!("[{names} /View]"),
        ]
        .into_iter()
        .chain((0..groups).map(|i| format!("<< /Type /OCG /Name (G{i}) /Intent 5 0 R >>")))
        .chain((0..groups).map(|i| format!("<< /OC {} 0 R >>", 6 + i)))
        .chain((0..aliases).map(|i| format!("<< /OC {} 0 R >>", first_alias + i)))
        .map(String::into_bytes)
        .collect();
        let pdf =
            Pdf::new(fixtures::file_with_aliases(objects, &vec![6; aliases])).expect("valid pdf");
        let state = OptionalContent::new(pdf.xref());
        for n in first_owner..first_alias {
            let owner = pdf
                .xref()
                .get::<Dict<'_>>(ObjectIdentifier::new(n as i32, 0))
                .expect("an owner");
            assert!(!state.shows(&owner), "{n}");
        }
        // Each group and its /Type, and the intents with each of their names once.
        assert_eq!(state.read.get(), 2 * groups + 1 + (intents + 1));
    }

    /// A list of groups many membership dictionaries share is read once.
    #[test]
    fn a_shared_list_of_groups_is_read_once() {
        let (groups, owners) = (1_000, 1_000);
        // 5 is the list, of group 4 and groups 6 …; then the owners.
        let first_owner = 6 + groups;
        let list = std::iter::once("4 0 R".to_owned())
            .chain((0..groups).map(|i| format!("{} 0 R", 6 + i)))
            .collect::<Vec<_>>()
            .join(" ");
        let objects = std::iter::once(format!("[{list}]"))
            .chain((0..groups).map(|i| format!("<< /Type /OCG /Name (G{i}) >>")))
            .chain(
                (0..owners).map(|_| "<< /OC << /Type /OCMD /OCGs 5 0 R /P /AllOn >> >>".to_owned()),
            );
        let pdf = file(objects, &[]);
        let (shown, evaluated) = shown(&pdf, first_owner..first_owner + owners);
        assert!(shown.iter().all(|on| !on));
        assert_eq!(evaluated as u32, owners + 1);
    }

    /// [`with_catalog`], whose table also lists entries at the places of `aliases`.
    fn with_catalog_and_aliases(
        catalog: &str,
        objects: impl IntoIterator<Item = String>,
        aliases: &[usize],
    ) -> Pdf {
        let all = [
            catalog.to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>".to_owned(),
            "<< /Type /OCG /Name (G) >>".to_owned(),
        ]
        .into_iter()
        .chain(objects)
        .map(String::into_bytes)
        .collect();
        Pdf::new(fixtures::file_with_aliases(all, aliases)).expect("valid pdf")
    }

    fn shown_in(pdf: &Pdf, owner: i32) -> bool {
        OptionalContent::new(pdf.xref()).shows(
            &pdf.xref()
                .get::<Dict<'_>>(ObjectIdentifier::new(owner, 0))
                .expect("an owner"),
        )
    }

    /// A file with `catalog`, objects from 5 on `objects`, and group 4 off by
    /// `/D` unless the catalog sets it otherwise.
    fn with_catalog(catalog: &str, objects: impl IntoIterator<Item = String>) -> Pdf {
        let all = [
            catalog.to_owned(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>".to_owned(),
            "<< /Type /OCG /Name (G) >>".to_owned(),
        ]
        .into_iter()
        .chain(objects)
        .map(String::into_bytes)
        .collect();
        Pdf::new(fixtures::file_with_aliases(all, &[])).expect("valid pdf")
    }

    /// One object read as a group's `/Usage` and as the `/View` of another's is
    /// read in each role, whichever group is found first.
    #[test]
    fn a_usage_and_a_view_of_one_object_are_read_apart() {
        let pdf = file(
            [
                "<< /ViewState /OFF >>".to_owned(),
                "<< /Type /OCG /Name (A) /Usage 5 0 R >>".to_owned(),
                "<< /Type /OCG /Name (B) /Usage << /View 5 0 R >> >>".to_owned(),
                "<< /OC 6 0 R >>".to_owned(),
                "<< /OC 7 0 R >>".to_owned(),
            ],
            &[],
        );
        assert_eq!(shown(&pdf, [8, 9]).0, [true, false]);
        assert_eq!(shown(&pdf, [9, 8]).0, [false, true]);
    }

    /// Usage applications that share a list of groups, or are one dictionary
    /// many entries of `/AS` refer to, apply each list once.
    #[test]
    fn shared_usage_applications_are_applied_once() {
        let (applications, groups) = (1_000, 1_000);
        let list = format!("[{}]", vec!["4 0 R"; groups].join(" "));
        let state = "/View << /ViewState /OFF >>";
        let shared = format!("<< /Event /View /OCGs 5 0 R {state} >>");
        let catalog = format!(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R] \
             /D << /ON [4 0 R] /AS [{} {}] >> >> >>",
            vec![shared; applications].join(" "),
            vec!["6 0 R"; applications].join(" "),
        );
        let pdf = with_catalog(
            &catalog,
            [
                list.clone(),
                format!("<< /OCGs {list} {state} >>"),
                "<< /OC 4 0 R >>".to_owned(),
            ],
        );
        let state = OptionalContent::new(pdf.xref());
        // Each list once, and the /Event and /ViewState of each usage application.
        assert_eq!(state.read.get(), 2 * groups + 2 * applications + 1);
        let owner = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(7, 0))
            .expect("an owner");
        assert!(!state.shows(&owner));
    }

    /// Configurations many entries of `/Configs` refer to, and an `/Intent` many
    /// configurations refer to, are read once.
    #[test]
    fn shared_configurations_are_read_once() {
        let (configs, intents) = (1_000, 1_000);
        let designs = vec!["/Design"; intents].join(" ");
        let catalog = format!(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R] \
             /D << /OFF [4 0 R] >> /Configs [{} {}] >> >>",
            vec!["5 0 R"; configs].join(" "),
            vec!["<< /Intent 6 0 R >>"; configs].join(" "),
        );
        let pdf = with_catalog(
            &catalog,
            [format!("<< /Intent [{designs}] >>"), format!("[{designs}]")],
        );
        let state = OptionalContent::new(pdf.xref());
        // Each `/Intent` with each of its names once.
        assert_eq!(state.read.get(), 2 * (1 + intents));
    }

    /// A group written in place in the document is read once however often what
    /// holds it is drawn, and its `/Intent` with it.
    #[test]
    fn a_group_written_in_place_is_read_once() {
        let intents = vec!["/Design"; 1_000].join(" ");
        let pdf = file(
            [format!(
                "<< /OC << /Type /OCG /Name (In place) /Intent [{intents} /View] >> >>"
            )],
            &[],
        );
        let state = OptionalContent::new(pdf.xref());
        let owner = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(5, 0))
            .expect("an owner");
        assert!((0..1_000).all(|_| state.shows(&owner)));
        // The group, its /Type, and its /Intent with each of its names.
        assert_eq!(state.read.get(), 1 + 1 + 1 + 1_001);
    }

    /// A group written in place in a list the catalog and a membership dictionary
    /// share is the group the configuration sets.
    #[test]
    fn a_group_written_in_place_in_a_shared_list_is_configured() {
        let pdf = with_catalog(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs 5 0 R \
             /D << /BaseState /OFF >> >> >>",
            [
                "[<< /Type /OCG /Name (In place) >>]".to_owned(),
                "<< /OC << /Type /OCMD /OCGs 5 0 R >> >>".to_owned(),
            ],
        );
        assert_eq!(shown(&pdf, [6]).0, [false]);
    }

    /// An entry of the table leading to a group's body is that group, which the
    /// configuration sets by any entry leading there.
    #[test]
    fn an_entry_leading_to_a_group_is_that_group() {
        // 8 is an entry at the place of group 4, 9 one at the place of group 5; /D
        // turns 4 off by itself and 5 by 9.
        let pdf = with_catalog_and_aliases(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R 5 0 R] \
             /D << /OFF [4 0 R 9 0 R] >> >> >>",
            [
                "<< /Type /OCG /Name (Five) >>".to_owned(),
                "<< /OC 8 0 R >>".to_owned(),
                "<< /OC 5 0 R >>".to_owned(),
            ],
            &[4, 5],
        );
        assert!(!shown_in(&pdf, 6));
        assert!(!shown_in(&pdf, 7));
    }

    /// Configurations, usage applications and lists of groups that entries of the
    /// table leading to one body share are read once, and a value many entries
    /// refer to once.
    #[test]
    fn configuration_values_shared_through_aliases_are_read_once() {
        let (entries, names) = (1_000, 1_000);
        let designs = vec!["/Design"; names].join(" ");
        let groups = vec!["4 0 R"; names].join(" ");
        // 5 is a configuration, 6 a usage application, 7 a list of groups, 8 a
        // long /Event; then the entries at the places of 5, 6 and 7.
        let first = 9;
        let at = |of: usize, i: usize| format!("{} 0 R", first + of * entries + i);
        let catalog = format!(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R] \
             /Configs [{}] /D << /AS [{} {}] >> >> >>",
            (0..entries).map(|i| at(0, i)).collect::<Vec<_>>().join(" "),
            (0..entries).map(|i| at(1, i)).collect::<Vec<_>>().join(" "),
            (0..entries)
                .map(|i| format!(
                    "<< /Event 8 0 R /OCGs {} {} >>",
                    at(2, i),
                    "/View << /ViewState /OFF >>"
                ))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let aliases: Vec<usize> = [5, 6, 7]
            .into_iter()
            .flat_map(|n| std::iter::repeat_n(n, entries))
            .collect();
        let pdf = with_catalog_and_aliases(
            &catalog,
            [
                format!("<< /Intent [{designs}] >>"),
                format!(
                    "<< /Event /View /OCGs {} 0 R /View << /ViewState /OFF >> >>",
                    first + 2 * entries
                ),
                format!("[{groups}]"),
                format!("({})", "x".repeat(10_000)),
            ],
            &aliases,
        );
        let state = OptionalContent::new(pdf.xref());
        // The configuration's /Intent with each of its names; the application's
        // /Event and /ViewState and each group of its list; the long /Event.
        assert_eq!(state.read.get(), (1 + names) + (2 + names) + 1);
    }

    /// What reading the configuration lost is told to each computation a group
    /// it sets is found for, however long after it was read.
    #[test]
    fn what_reading_the_configuration_lost_is_told_where_it_is_used() {
        let pdf = with_catalog(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R] \
             /D << /OFF 6 0 R >> >> >>",
            ["<< /OC 4 0 R >>".to_owned(), "<< /Broken >".to_owned()],
        );
        let (state, lost) = pdf.xref().watch(|| OptionalContent::new(pdf.xref()));
        assert_eq!(lost, None, "reading it is told nothing");
        let owner = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(5, 0))
            .expect("an owner");
        for _ in 0..2 {
            let (shown, lost) = pdf.xref().watch(|| state.shows(&owner));
            assert!(shown);
            assert_eq!(lost.map(|id| id.obj_number), Some(6));
        }
    }

    /// A group the catalog does not list is on before the configuration is read:
    /// what reading it lost is told only where a listed group is used.
    #[test]
    fn what_reading_the_configuration_lost_is_not_told_by_groups_it_does_not_set() {
        let pdf = with_catalog(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R] \
             /D << /OFF 8 0 R >> >> >>",
            [
                "<< /OC 4 0 R >>".to_owned(),
                "<< /Type /OCG /Name (Unlisted) >>".to_owned(),
                "<< /OC 6 0 R >>".to_owned(),
                "<< /Broken >".to_owned(),
            ],
        );
        let state = OptionalContent::new(pdf.xref());
        let owner = |n| {
            pdf.xref()
                .get::<Dict<'_>>(ObjectIdentifier::new(n, 0))
                .expect("an owner")
        };
        for _ in 0..2 {
            assert_eq!(pdf.xref().watch(|| state.shows(&owner(7))), (true, None));
            let (shown, lost) = pdf.xref().watch(|| state.shows(&owner(5)));
            assert!(shown);
            assert_eq!(lost.map(|id| id.obj_number), Some(8));
        }
    }

    /// Content in a level that does not show is not shown, whatever the optional
    /// content it begins: what deciding that would lose is not told.
    #[test]
    fn optional_content_inside_content_not_shown_is_not_decided() {
        let pdf = with_catalog(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R] \
             /D << /OFF [4 0 R] >> >> >>",
            [
                "<< /Off 4 0 R /Bad 6 0 R /On 8 0 R >>".to_owned(),
                "<< /Type /OCG /Intent 7 0 R >>".to_owned(),
                "<< /Broken >".to_owned(),
                "<< /Type /OCG /Name (On) >>".to_owned(),
            ],
        );
        let properties = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(5, 0))
            .expect("a property list");
        let owner = super::Kept::Document(properties);
        for (outer, told) in [(&b"Off"[..], None), (&b"On"[..], Some(7))] {
            let content = Rc::new(OptionalContent::new(pdf.xref()));
            let mut state = super::OcgState::new(Some(content));
            let (_, lost) = pdf.xref().watch(|| {
                assert!(state.begin_named(&owner, outer));
                assert!(state.begin_named(&owner, b"Bad"));
            });
            assert_eq!(lost.map(|id| id.obj_number), told);
        }
    }

    /// A group that is a stream is read by its dictionary alone: what reading its
    /// data would lose is not told.
    #[test]
    fn a_group_that_is_a_stream_is_read_without_its_data() {
        let pdf = with_catalog(
            "<< /Type /Catalog /Pages 2 0 R /OCProperties << /OCGs [4 0 R] \
             /D << /OFF [4 0 R] >> >> >>",
            [
                "<< /OC 6 0 R >>".to_owned(),
                "<< /Type /OCMD /OCGs [4 0 R] /Length 7 0 R >>\nstream\nx\nendstream".to_owned(),
                "<< /Broken >".to_owned(),
            ],
        );
        let state = OptionalContent::new(pdf.xref());
        let owner = pdf
            .xref()
            .get::<Dict<'_>>(ObjectIdentifier::new(5, 0))
            .expect("an owner");
        let (shown, lost) = pdf.xref().watch(|| state.shows(&owner));
        assert!(!shown);
        assert_eq!(lost, None);
    }

    /// The contents drawn from one document's cache, a page's, a pattern's or a
    /// glyph's, and its annotations share one optional content.
    #[test]
    fn the_contents_of_a_document_share_its_optional_content() {
        let pdf = file([], &[]);
        let fields = Fields::default();
        assert!(Rc::ptr_eq(
            &fields.optional_content(pdf.xref()),
            &fields.optional_content(pdf.xref())
        ));
        let other = file([], &[]);
        assert!(fields.optional_content(other.xref()).of(other.xref()));
    }
}

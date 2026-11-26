use hayro_syntax::object::{Array, Dict, MaybeRef, Object, ObjectIdentifier};
use hayro_syntax::xref::XRef;
use std::collections::{HashMap, HashSet};

const MAX_EXPRESSION_DEPTH: u8 = 32;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum GroupId {
    Reference(ObjectIdentifier),
    ArrayItem(ObjectIdentifier, usize),
}

#[derive(Default)]
pub(crate) struct OcgState {
    pub(crate) viewing: Option<std::rc::Rc<ViewingState>>,
    visibility_stack: Vec<bool>,
    floor: usize,
}

#[derive(Default)]
pub(crate) struct ViewingState {
    hidden: HashSet<GroupId>,
}

fn resolve<'a>(xref: &'a XRef, raw: MaybeRef<Object<'a>>) -> Option<Object<'a>> {
    match raw {
        MaybeRef::Ref(r) => xref.get(r.into()),
        MaybeRef::NotRef(obj) => Some(obj),
    }
}

fn dictionary<'a>(xref: &'a XRef, dict: &Dict<'a>, key: &[u8]) -> Option<Dict<'a>> {
    match dict.get_raw::<Dict<'_>>(key)? {
        MaybeRef::Ref(r) => xref.get(r.into()),
        MaybeRef::NotRef(d) => Some(d),
    }
}

fn word<'a>(obj: &'a Object<'_>) -> &'a [u8] {
    match obj {
        Object::Name(n) => n.as_ref(),
        Object::String(s) => s.as_bytes(),
        _ => b"",
    }
}

fn entry_is(dict: &Dict<'_>, key: &[u8], value: &[u8]) -> Option<bool> {
    if !dict.contains_key(key) {
        return None;
    }
    Some(
        dict.get::<Object<'_>>(key)
            .is_some_and(|o| word(&o) == value),
    )
}

fn views(xref: &XRef, dict: &Dict<'_>) -> Option<bool> {
    let view = |obj: &Object<'_>| matches!(word(obj), b"View" | b"All");
    Some(match resolve(xref, dict.get_raw(b"Intent")?)? {
        Object::Array(a) => a
            .raw_iter()
            .filter_map(|i| resolve(xref, i))
            .any(|o| view(&o)),
        other => view(&other),
    })
}

fn group_id(
    item: &MaybeRef<Object<'_>>,
    list: Option<ObjectIdentifier>,
    index: usize,
) -> Option<GroupId> {
    if let Some(r) = item.as_obj_ref() {
        Some(GroupId::Reference(r.into()))
    } else if matches!(item, MaybeRef::NotRef(Object::Dict(_))) {
        list.map(|id| GroupId::ArrayItem(id, index))
    } else {
        None
    }
}

fn set_groups(dict: &Dict<'_>, key: &[u8], mut set: impl FnMut(GroupId)) {
    if let Some(array) = dict.get::<Array<'_>>(key) {
        let list = dict.get_ref(key).map(Into::into);
        for (index, item) in array.raw_iter().enumerate() {
            if let Some(id) = group_id(&item, list, index) {
                set(id);
            }
        }
    }
}

impl ViewingState {
    pub(crate) fn from_catalog(xref: &XRef, catalog: &Dict<'_>) -> Self {
        let mut configured = HashMap::new();
        let Some(props) = catalog.get::<Dict<'_>>(b"OCProperties") else {
            return Self::default();
        };
        let config = props
            .get::<Array<'_>>(b"Configs")
            .and_then(|a| a.dictionaries().find(|c| views(xref, c) == Some(true)))
            .or_else(|| props.get::<Dict<'_>>(b"D"));
        let Some(config) = config else {
            return Self::default();
        };
        let base = entry_is(&config, b"BaseState", b"OFF") != Some(true);
        set_groups(&props, b"OCGs", |id| {
            configured.insert(id, base);
        });
        let mut apply = |dict: &Dict<'_>, key: &[u8], on| {
            set_groups(dict, key, |id| {
                if let Some(value) = configured.get_mut(&id) {
                    *value = on;
                }
            });
        };
        apply(&config, b"ON", true);
        apply(&config, b"OFF", false);
        if let Some(usages) = config.get::<Array<'_>>(b"AS") {
            for usage in usages.dictionaries() {
                if entry_is(&usage, b"Event", b"View") != Some(false)
                    && let Some(view) = usage.get::<Dict<'_>>(b"View")
                {
                    apply(
                        &usage,
                        b"OCGs",
                        entry_is(&view, b"ViewState", b"OFF") != Some(true),
                    );
                }
            }
        }
        // A stream and its dictionary are distinct PDF objects.
        let hidden = configured
            .into_iter()
            .filter_map(|(id, on)| {
                if on {
                    return None;
                }
                match id {
                    GroupId::Reference(r)
                        if !matches!(xref.get::<Object<'_>>(r), Some(Object::Dict(_))) =>
                    {
                        None
                    }
                    _ => Some(id),
                }
            })
            .collect();
        Self { hidden }
    }

    fn group(&self, xref: &XRef, id: Option<GroupId>, dict: &Dict<'_>) -> bool {
        if views(xref, dict) == Some(false) {
            return true;
        }
        let usage = dictionary(xref, dict, b"Usage").and_then(|u| dictionary(xref, &u, b"View"));
        if let Some(off) = usage.and_then(|v| entry_is(&v, b"ViewState", b"OFF")) {
            return !off;
        }
        id.is_none_or(|k| !self.hidden.contains(&k))
    }

    fn expression(
        &self,
        xref: &XRef,
        expr: &Array<'_>,
        remaining: u8,
        memo: &mut HashMap<(ObjectIdentifier, u8), bool>,
    ) -> bool {
        let mut items = expr.raw_iter();
        let Some(op) = items.next().and_then(|i| resolve(xref, i)) else {
            return false;
        };
        let op = word(&op);
        if !matches!(op, b"Not" | b"And" | b"Or") {
            return false;
        }
        let mut result = false;
        for (index, item) in items.enumerate() {
            let id = item.as_obj_ref().map(Into::into);
            let key = id.map(|id| (id, remaining));
            let on = if let Some(on) = key.and_then(|k| memo.get(&k)) {
                *on
            } else {
                match resolve(xref, item) {
                    Some(Object::Dict(d)) => self.group(xref, id.map(GroupId::Reference), &d),
                    Some(Object::Array(a)) => {
                        let on = remaining > 0 && self.expression(xref, &a, remaining - 1, memo);
                        if let Some(k) = key {
                            memo.insert(k, on);
                        }
                        on
                    }
                    _ if op == b"Not" => return false,
                    None => continue,
                    _ => false,
                }
            };
            result = match op {
                b"Not" => return !on,
                _ if index == 0 => on,
                b"And" => result && on,
                _ => result || on,
            };
            if (op == b"And" && !result) || (op == b"Or" && result) {
                return result;
            }
        }
        result
    }

    fn membership(&self, xref: &XRef, dict: &Dict<'_>) -> bool {
        if let Some(expr) = dict.get::<Array<'_>>(b"VE") {
            return self.expression(xref, &expr, MAX_EXPRESSION_DEPTH, &mut HashMap::new());
        }
        let list = dict.get_ref(b"OCGs").map(Into::into);
        let Some(obj) = dict.get_raw(b"OCGs").and_then(|raw| resolve(xref, raw)) else {
            return true;
        };
        let array = match obj {
            Object::Dict(d) => return self.group(xref, list.map(GroupId::Reference), &d),
            Object::Array(a) => a,
            _ => return true,
        };
        let (mut on, mut off) = (false, false);
        for (index, item) in array.raw_iter().enumerate() {
            let id = group_id(&item, list, index);
            let d = match item {
                MaybeRef::Ref(r) => xref.get::<Dict<'_>>(r.into()),
                MaybeRef::NotRef(Object::Dict(d)) => Some(d),
                _ => None,
            };
            if let Some(group) = d {
                if self.group(xref, id, &group) {
                    on = true;
                } else {
                    off = true;
                }
            }
        }
        if !on && !off {
            return true;
        }
        let policy = dict.get::<Object<'_>>(b"P");
        match policy.as_ref().map(word) {
            None if dict.get_ref(b"P").is_some() => false,
            None | Some(b"AnyOn") => on,
            Some(b"AllOn") => !off,
            Some(b"AllOff") => !on,
            Some(b"AnyOff") => off,
            _ => false,
        }
    }
}

impl OcgState {
    pub(crate) fn begin_named(&mut self, xref: &XRef, owner: &Dict<'_>, key: &[u8]) -> bool {
        let Some(viewing) = &self.viewing else {
            return false;
        };
        let Some(dict) = dictionary(xref, owner, key) else {
            return false;
        };
        let visible = self.is_visible()
            && if entry_is(&dict, b"Type", b"OCG") != Some(false) {
                viewing.group(
                    xref,
                    owner.get_ref(key).map(|r| GroupId::Reference(r.into())),
                    &dict,
                )
            } else {
                viewing.membership(xref, &dict)
            };
        self.visibility_stack.push(visible);
        true
    }

    pub(crate) fn open(&mut self) -> usize {
        core::mem::replace(&mut self.floor, self.visibility_stack.len())
    }

    pub(crate) fn close(&mut self, floor: usize) {
        self.visibility_stack.truncate(self.floor);
        self.floor = floor;
    }

    pub(crate) fn begin_marked_content(&mut self) {
        self.visibility_stack.push(self.is_visible());
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

use hayro_syntax::object::array::ArrayIter;
use hayro_syntax::object::dict::keys::{
    BASE_STATE, D, INTENT, OCG, OCGS, OCMD, OCPROPERTIES, OFF, ON, P, TYPE, VIEW,
};
use hayro_syntax::object::{Array, Dict, MaybeRef, Name, Object, ObjectIdentifier};
use hayro_syntax::xref::XRef;
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};

#[derive(Default)]
pub(crate) struct OcgState<'a> {
    configured_xref: Option<&'a XRef>,
    inactive_ocgs: HashSet<ObjectIdentifier>,
    intents: HashSet<Name<'a>>,
    members: HashMap<ObjectIdentifier, Member<'a>>,
    visibility_stack: Vec<bool>,
}

impl<'a> OcgState<'a> {
    pub(crate) fn from_catalog(catalog: &Dict<'a>, xref: &'a XRef) -> Self {
        let Some(oc_properties) = catalog.get::<Dict<'_>>(OCPROPERTIES) else {
            return Self::default();
        };

        let config = oc_properties.get::<Dict<'_>>(D).unwrap_or_default();

        let mut inactive = HashSet::new();

        let base_state = config
            .get::<Name<'_>>(BASE_STATE)
            .and_then(|b| BaseState::from_name(b.as_ref()));

        if base_state.unwrap_or(BaseState::On) == BaseState::Off
            && let Some(ocgs) = oc_properties.get::<Array<'_>>(OCGS)
        {
            for item in ocgs.raw_iter() {
                if let Some(ref_) = item.as_obj_ref() {
                    let id: ObjectIdentifier = ref_.into();
                    inactive.insert(id);
                }
            }
        }

        let mut read_ocg_array = |key, insert_active: bool| {
            if let Some(arr) = config.get::<Array<'_>>(key) {
                for item in arr.raw_iter() {
                    if let Some(ref_) = item.as_obj_ref() {
                        let id: ObjectIdentifier = ref_.into();
                        if insert_active {
                            inactive.remove(&id);
                        } else {
                            inactive.insert(id);
                        }
                    }
                }
            }
        };

        read_ocg_array(ON, true);
        read_ocg_array(OFF, false);

        let intents = match config.get::<Object<'_>>(INTENT) {
            Some(Object::Array(array)) => array.iter::<Name<'_>>().collect(),
            Some(Object::Name(name)) => HashSet::from([name]),
            _ => HashSet::from([Name::new_unescaped(VIEW)]),
        };
        Self {
            intents,
            configured_xref: Some(xref),
            inactive_ocgs: inactive,
            ..Self::default()
        }
    }

    pub(crate) fn is_configured(&self) -> bool {
        self.configured_xref.is_some()
    }

    fn member<'b>(
        &mut self,
        object: MaybeRef<Object<'b>>,
        xref: &'a XRef,
        missed: &mut HashSet<ObjectIdentifier>,
    ) -> Member<'b>
    where
        'a: 'b,
    {
        let id = match object {
            MaybeRef::Ref(r) => r.into(),
            MaybeRef::NotRef(Object::Array(array)) => return Member::Array(array),
            _ => return Member::Ignored,
        };
        let entry = match self.members.entry(id) {
            Entry::Occupied(entry) => return entry.get().clone(),
            Entry::Vacant(entry) => entry,
        };
        if missed.contains(&id) {
            return Member::Ignored;
        }
        let Some(object) = xref.get::<Object<'_>>(id) else {
            missed.insert(id);
            return Member::Ignored;
        };
        entry
            .insert(match object {
                Object::Dict(dict) if is_applicable(&dict, &self.intents) => {
                    Member::Group(!self.inactive_ocgs.contains(&id))
                }
                Object::Array(array) => Member::Array(array),
                _ => Member::Ignored,
            })
            .clone()
    }

    fn ocmd_is_active(&mut self, ocmd: &Dict<'_>, xref: &'a XRef) -> bool {
        let mut missed = HashSet::new();
        if let Some(array) = ocmd.get::<Array<'_>>(b"VE")
            && let Some(visible) = self.expression(array, xref, &mut missed)
        {
            return visible;
        }

        let policy = ocmd
            .get::<Name<'_>>(P)
            .and_then(|n| OcmdPolicy::from_name(n.as_ref()))
            .unwrap_or(OcmdPolicy::AnyOn);
        let Some(object) = ocmd.get_raw::<Object<'_>>(OCGS) else {
            return true;
        };
        let members = match self.member(object, xref, &mut missed) {
            Member::Array(array) => array,
            Member::Group(active) => {
                return active == matches!(policy, OcmdPolicy::AnyOn | OcmdPolicy::AllOn);
            }
            Member::Ignored => return true,
        };
        let mut states = members
            .raw_iter()
            .filter_map(|item| match self.member(item, xref, &mut missed) {
                Member::Group(active) => Some(active),
                _ => None,
            })
            .peekable();
        if states.peek().is_none() {
            return true;
        }
        match policy {
            OcmdPolicy::AnyOn => states.any(|on| on),
            OcmdPolicy::AnyOff => states.any(|on| !on),
            OcmdPolicy::AllOn => states.all(|on| on),
            OcmdPolicy::AllOff => states.all(|on| !on),
        }
    }

    fn expression(
        &mut self,
        array: Array<'_>,
        xref: &'a XRef,
        missed: &mut HashSet<ObjectIdentifier>,
    ) -> Option<bool> {
        const MAX_DEPTH: usize = 32;
        const MAX_OPERANDS: usize = 4096;

        let mut stack = vec![Frame::new(array, xref)?];
        let mut operands = 0;
        loop {
            let frame = stack.last_mut()?;
            if let Some(item) = frame.items.next() {
                operands += 1;
                if operands > MAX_OPERANDS {
                    return None;
                }
                frame.has_operand = true;
                match self.member(item, xref, missed) {
                    Member::Array(arr) => {
                        if stack.len() >= MAX_DEPTH {
                            return None;
                        }
                        stack.push(Frame::new(arr, xref)?);
                    }
                    Member::Group(value) => frame.combine(value)?,
                    Member::Ignored => {}
                }
            } else {
                let frame = stack.pop()?;
                if !frame.has_operand {
                    return None;
                }
                let Some(parent) = stack.last_mut() else {
                    return Some(frame.value.unwrap_or(true));
                };
                if let Some(value) = frame.value {
                    parent.combine(value)?;
                }
            }
        }
    }

    pub(crate) fn oc_is_active(
        &mut self,
        props: &Dict<'_>,
        ref_id: Option<ObjectIdentifier>,
    ) -> bool {
        let Some(xref) = self.configured_xref else {
            return true;
        };
        if props.get::<Name<'_>>(TYPE).as_deref() == Some(OCMD) {
            self.ocmd_is_active(props, xref)
        } else {
            !ref_id.is_some_and(|id| {
                self.inactive_ocgs.contains(&id) && is_applicable(props, &self.intents)
            })
        }
    }

    pub(crate) fn depth(&self) -> usize {
        self.visibility_stack.len()
    }

    pub(crate) fn begin_ocg(&mut self, props: &Dict<'_>, ref_id: Option<ObjectIdentifier>) {
        let visible = self.is_visible() && self.oc_is_active(props, ref_id);
        self.visibility_stack.push(visible);
    }

    pub(crate) fn begin_marked_content(&mut self) {
        let visible = self.is_visible();
        self.visibility_stack.push(visible);
    }

    pub(crate) fn end_marked_content(&mut self) {
        self.visibility_stack.pop();
    }

    pub(crate) fn is_visible(&self) -> bool {
        self.visibility_stack.last().copied().unwrap_or(true)
    }
}

fn is_applicable(dict: &Dict<'_>, intents: &HashSet<Name<'_>>) -> bool {
    dict.get::<Name<'_>>(TYPE).as_deref() == Some(OCG)
        && (intents.contains(b"All".as_slice())
            || match dict.get::<Object<'_>>(INTENT) {
                Some(Object::Name(name)) => intents.contains(name.as_ref()),
                Some(Object::Array(names)) => names
                    .iter::<Name<'_>>()
                    .any(|name| intents.contains(name.as_ref())),
                _ => intents.contains(VIEW),
            })
}

#[derive(Clone)]
enum Member<'a> {
    Group(bool),
    Array(Array<'a>),
    Ignored,
}

#[derive(PartialEq)]
enum Operator {
    And,
    Or,
    Not,
}

struct Frame<'a> {
    items: ArrayIter<'a>,
    op: Operator,
    has_operand: bool,
    value: Option<bool>,
}

impl<'a> Frame<'a> {
    fn new(array: Array<'a>, xref: &'a XRef) -> Option<Self> {
        let mut items = array.raw_iter();
        let name = match items.next()? {
            MaybeRef::NotRef(Object::Name(name)) => name,
            MaybeRef::Ref(r) => xref.get::<Name<'_>>(r.into())?,
            _ => return None,
        };
        let op = match name.as_ref() {
            b"And" => Operator::And,
            b"Or" => Operator::Or,
            b"Not" => Operator::Not,
            _ => return None,
        };
        Some(Self {
            items,
            op,
            has_operand: false,
            value: None,
        })
    }

    fn combine(&mut self, value: bool) -> Option<()> {
        self.value = Some(match self.op {
            Operator::And => self.value.unwrap_or(true) && value,
            Operator::Or => self.value.unwrap_or(false) || value,
            Operator::Not if self.value.is_some() => return None,
            Operator::Not => !value,
        });
        Some(())
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum BaseState {
    On,
    Off,
    Unchanged,
}

impl BaseState {
    fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"ON" => Some(Self::On),
            b"OFF" => Some(Self::Off),
            b"Unchanged" => Some(Self::Unchanged),
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Copy, Clone)]
enum OcmdPolicy {
    AllOn,
    AnyOn,
    AnyOff,
    AllOff,
}

impl OcmdPolicy {
    fn from_name(name: &[u8]) -> Option<Self> {
        match name {
            b"AllOn" => Some(Self::AllOn),
            b"AnyOn" => Some(Self::AnyOn),
            b"AnyOff" => Some(Self::AnyOff),
            b"AllOff" => Some(Self::AllOff),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hayro_syntax::Pdf;

    use hayro_syntax::object::FromBytes;

    fn fixture() -> Pdf {
        Pdf::new(include_bytes!("../tests/data/annotation_oc.pdf").to_vec()).unwrap()
    }

    #[test]
    fn repeated_ocmd_members_share_lookups_across_evaluations() {
        let pdf = fixture();
        let xref = pdf.xref();
        let catalog = xref.get::<Dict<'_>>(xref.root_id()).unwrap();
        let mut state = OcgState::from_catalog(&catalog, xref);
        let members = "4 0 R 9 0 R 10 0 R ".repeat(100);
        let data = format!("<< /Type /OCMD /OCGs [{members}] >>");
        let ocmd = Dict::from_bytes(data.as_bytes()).unwrap();
        for _ in 0..10 {
            assert!(!state.oc_is_active(&ocmd, None));
            assert_eq!(state.members.len(), 2);
        }
    }

    fn replace_bytes(bytes: &mut [u8], old: &[u8], new: &[u8]) {
        let offset = bytes.windows(old.len()).position(|w| w == old).unwrap();
        bytes[offset..offset + old.len()].copy_from_slice(new);
    }

    #[test]
    fn missing_member_recovers_after_xref_repair() {
        let mut bytes = include_bytes!("../tests/data/annotation_oc.pdf").to_vec();
        replace_bytes(&mut bytes, b"0000000500 00000 n", b"0000000000 00000 f");
        replace_bytes(&mut bytes, b"0000000568 00000 n", b"0000000500 00000 n");
        let pdf = Pdf::new(bytes).unwrap();
        let xref = pdf.xref();
        let catalog = xref.get::<Dict<'_>>(xref.root_id()).unwrap();
        let mut state = OcgState::from_catalog(&catalog, xref);
        let ocmd = Dict::from_bytes(b"<< /Type /OCMD /OCGs [4 0 R 4 0 R] >>").unwrap();

        assert!(state.oc_is_active(&ocmd, None));
        xref.get::<Dict<'_>>(ObjectIdentifier::new(5, 0)).unwrap();
        assert!(!state.oc_is_active(&ocmd, None));
    }

    #[test]
    fn expression_limits_and_malformed_operators_use_policy() {
        let mut bytes = include_bytes!("../tests/data/annotation_oc.pdf").to_vec();
        replace_bytes(&mut bytes, b"9 0 obj\nnull", b"9 0 obj\n/Not");
        let pdf = Pdf::new(bytes).unwrap();
        let xref = pdf.xref();
        let catalog = xref.get::<Dict<'_>>(xref.root_id()).unwrap();
        let mut state = OcgState::from_catalog(&catalog, xref);
        let nested = |n| format!("{}5 0 R{}", "[/And ".repeat(n), "]".repeat(n));
        let wide = |n| format!("[/Or {}]", "5 0 R ".repeat(n));
        for (expr, visible) in [
            ("[9 0 R 4 0 R]".into(), true),
            (nested(32), true),
            (nested(33), false),
            (wide(4096), true),
            (wide(4097), false),
            ("[/Or 5 0 R [/Unknown 4 0 R]]".into(), false),
            ("[/Not 4 0 R 4 0 R]".into(), false),
            ("[/And]".into(), false),
            ("[/Not null 4 0 R 9 0 R]".into(), true),
        ] {
            let bytes = format!("<< /Type /OCMD /VE {expr} /OCGs [4 0 R] >>");
            let ocmd = Dict::from_bytes(bytes.as_bytes()).unwrap();
            assert_eq!(state.oc_is_active(&ocmd, None), visible);
        }
    }
}

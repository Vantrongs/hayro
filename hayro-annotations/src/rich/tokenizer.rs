use super::{Dom, Unsupported};
use html5ever::tendril::StrTendril;
use html5ever::tokenizer::states::RawKind;
use html5ever::tokenizer::{Doctype, Tag, TagKind, Token, TokenSink, TokenSinkResult};
use html5ever::tree_builder::TreeBuilder;
use html5ever::{Attribute, LocalName, QualName};
use html5gum::emitters::callback::{CallbackEmitter, CallbackEvent};
use html5gum::{Span, State, Tokenizer};
use std::collections::{HashMap, hash_map::RandomState};
use std::hash::{BuildHasher, Hash, Hasher};

#[derive(Default, PartialEq, Eq)]
struct Ignored {
    pairs: HashMap<Vec<u8>, Vec<u8>>,
    fingerprint: u64,
}

impl Hash for Ignored {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.fingerprint.hash(state);
    }
}

#[derive(Default)]
struct Tags {
    tag: Option<Tag>,
    attr: Option<Vec<u8>>,
    value: Vec<u8>,
    ignored: Ignored,
    sets: HashMap<Ignored, usize>,
    hash: RandomState,
}

fn string(bytes: &[u8]) -> StrTendril {
    std::str::from_utf8(bytes)
        .expect("tokenized UTF-8 input")
        .into()
}

fn tag(name: &[u8], kind: TagKind) -> Tag {
    // Static HTML names retain their tree-builder recovery behavior, including ignored stray tags.
    let name = std::str::from_utf8(name)
        .ok()
        .and_then(|s| {
            if s.len() <= 7 {
                Some(LocalName::from(s))
            } else {
                LocalName::try_static(s)
            }
        })
        .unwrap_or_else(|| LocalName::from("unknown"));
    Tag {
        name,
        kind,
        self_closing: false,
        attrs: Vec::new(),
        had_duplicate_attributes: false,
    }
}

fn relevant(name: &[u8]) -> bool {
    matches!(
        name,
        b"style" | b"dir" | b"align" | b"hidden" | b"bgcolor" | b"background" | b"text"
    )
}

impl Tags {
    fn attribute(&mut self) {
        let Some(name) = self.attr.take() else { return };
        let Some(tag) = &mut self.tag else { return };
        if relevant(&name) {
            let local = LocalName::from(std::str::from_utf8(&name).unwrap());
            if !tag.attrs.iter().any(|a| a.name.local == local) {
                tag.attrs.push(Attribute {
                    name: QualName::new(None, Default::default(), local),
                    value: string(&self.value),
                });
            }
        } else if matches!(&*tag.name, "b" | "strong" | "i" | "em")
            && !self.ignored.pairs.contains_key(&name)
        {
            self.ignored.fingerprint ^= self.hash.hash_one((&name, &self.value));
            self.ignored
                .pairs
                .insert(name, std::mem::take(&mut self.value));
        }
        self.value.clear();
    }

    fn token(&mut self, event: CallbackEvent<'_>) -> Option<Token> {
        match event {
            CallbackEvent::OpenStartTag { name } => self.tag = Some(tag(name, TagKind::StartTag)),
            CallbackEvent::AttributeName { name } => {
                self.attribute();
                if self.tag.as_ref().is_some_and(|t| {
                    relevant(name) || matches!(&*t.name, "b" | "strong" | "i" | "em")
                }) {
                    self.attr = Some(name.into());
                }
            }
            CallbackEvent::AttributeValue { value } => {
                if self.attr.is_some() {
                    self.value.extend_from_slice(value);
                }
            }
            CallbackEvent::CloseStartTag { self_closing } => {
                self.attribute();
                let mut tag = self.tag.take()?;
                tag.self_closing = self_closing;
                if !self.ignored.pairs.is_empty() {
                    let next = self.sets.len();
                    let id = self
                        .sets
                        .entry(std::mem::take(&mut self.ignored))
                        .or_insert(next);
                    tag.attrs.push(Attribute {
                        name: QualName::new(None, Default::default(), LocalName::from("id")),
                        value: id.to_string().into(),
                    });
                }
                return Some(Token::TagToken(tag));
            }
            CallbackEvent::EndTag { name } => {
                return Some(Token::TagToken(tag(name, TagKind::EndTag)));
            }
            CallbackEvent::String { value } => return Some(Token::CharacterTokens(string(value))),
            CallbackEvent::Doctype {
                name,
                public_identifier,
                system_identifier,
                force_quirks,
            } => {
                return Some(Token::DoctypeToken(Doctype {
                    name: Some(string(name)),
                    public_id: public_identifier.map(string),
                    system_id: system_identifier.map(string),
                    force_quirks,
                }));
            }
            CallbackEvent::Comment { .. } | CallbackEvent::ProcessingInstruction { .. } => {
                return Some(Token::CommentToken(StrTendril::new()));
            }
            CallbackEvent::Error(_) => {}
        }
        None
    }
}

pub(super) fn parse(input: &str, dom: Dom) -> Result<Dom, Unsupported> {
    let tree = TreeBuilder::new(dom, Default::default());
    let mut tags = Tags::default();
    let callback = CallbackEmitter::new(|event: CallbackEvent<'_>, _: Span<()>| tags.token(event));
    let mut tokens = Tokenizer::new_with_emitter(input, callback);
    while let Some(Ok(token)) = tokens.next() {
        let result = match token {
            Token::CharacterTokens(text) if text.contains('\0') => {
                for (i, part) in text.split('\0').enumerate() {
                    if i != 0 {
                        let _ = tree.process_token(Token::NullCharacterToken, 1);
                    }
                    if !part.is_empty() {
                        let _ = tree.process_token(Token::CharacterTokens(part.into()), 1);
                    }
                }
                TokenSinkResult::Continue
            }
            token => tree.process_token(token, 1),
        };
        if tree.sink.unsupported.get() {
            return Err(Unsupported::RichText);
        }
        match result {
            TokenSinkResult::RawData(RawKind::Rawtext) => tokens.set_state(State::RawText),
            TokenSinkResult::RawData(RawKind::Rcdata) => tokens.set_state(State::RcData),
            TokenSinkResult::RawData(RawKind::ScriptData) => tokens.set_state(State::ScriptData),
            TokenSinkResult::Plaintext => tokens.set_state(State::PlainText),
            TokenSinkResult::RawData(RawKind::ScriptDataEscaped(_)) => {
                return Err(Unsupported::RichText);
            }
            _ => {}
        }
    }
    let _ = tree.process_token(Token::EOFToken, 1);
    tree.end();
    Ok(tree.sink)
}

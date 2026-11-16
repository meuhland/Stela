//! Enough of the Itanium C++ ABI's name mangling (5.1) to compare two
//! methods by what a vtable slot dispatches on: the method's own name, its
//! `const`, and its parameter types, each with every substitution (`S_`,
//! `S0_`, ...) expanded. The class a method belongs to is left out, so a
//! kext's override and the kernel method it replaces compare equal:
//! `__ZN9IOService5startEPS_` and `__ZN15StelaVirtIOMMIO5startEP9IOService`
//! both take `P9IOService`.
//!
//! The grammar covers what IOKit's virtual methods use: nested names with
//! their CV-qualifiers, constructors and destructors, operator names, the
//! builtin types, CV-qualified, pointer, reference, function, array and
//! member-pointer types, class names nested or templated, template
//! arguments of types and literals, and the `S` substitutions with the
//! `std` abbreviations. Template parameters, packs, `decltype`,
//! expressions, ABI tags and local names are not: a symbol that uses one
//! has no [`Signature`], and a caller treats the method as unchecked.
use alloc::{format, string::String, vec::Vec};

/// What a vtable slot's method is to the caller: its unqualified name as
/// mangled (`5start`, `D1`, `C2`, `nw`), whether it is `const`, and its
/// parameters' types mangled with no substitution left.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signature {
    pub name: String,
    pub constant: bool,
    pub parameters: Vec<String>,
}

/// The signature of a Mach-O symbol (one leading `_` before `_Z`), or
/// `None` for a name that is not a mangled function this grammar covers,
/// such as `___cxa_pure_virtual`.
pub fn signature(symbol: &str) -> Option<Signature> {
    let mangled = symbol.strip_prefix('_').unwrap_or(symbol);
    let encoding = mangled.strip_prefix("_Z")?;
    let mut parser = Parser {
        text: encoding.as_bytes(),
        at: 0,
        substitutions: Vec::new(),
    };
    let (name, constant) = parser.function_name()?;
    let mut parameters = Vec::new();
    while parser.at < parser.text.len() {
        parameters.push(parser.ty()?);
    }
    if parameters.is_empty() {
        // A function always has a parameter list; `v` alone is an empty one.
        return None;
    }
    if parameters == ["v"] {
        parameters.clear();
    }
    Some(Signature {
        name,
        constant,
        parameters,
    })
}

struct Parser<'a> {
    text: &'a [u8],
    at: usize,
    /// The substitution candidates so far, each fully expanded.
    substitutions: Vec<String>,
}

impl Parser<'_> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<u8> {
        self.text.get(self.at + ahead).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.at += 1;
        }
        found
    }

    fn candidate(&mut self, expanded: &str) {
        self.substitutions.push(expanded.into());
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.at;
        while self.peek().is_some_and(|byte| byte.is_ascii_digit()) {
            self.at += 1;
        }
        core::str::from_utf8(&self.text[start..self.at])
            .ok()?
            .parse()
            .ok()
    }

    /// `<source-name>`: a length and that many bytes, kept as mangled.
    fn source_name(&mut self) -> Option<String> {
        let length = self.number()?;
        let end = self.at.checked_add(length)?;
        let name = core::str::from_utf8(self.text.get(self.at..end)?).ok()?;
        self.at = end;
        Some(format!("{length}{name}"))
    }

    /// `<unqualified-name>`: a source name, a constructor or destructor,
    /// or a two-letter operator.
    fn unqualified_name(&mut self) -> Option<String> {
        match self.peek()? {
            b'0'..=b'9' => self.source_name(),
            b'C' => {
                // C1, C2, C3, CI1 <type>, CI2 <type>: no inheriting
                // constructors here.
                let kind = self.peek_at(1)?;
                if !matches!(kind, b'1'..=b'5') {
                    return None;
                }
                self.at += 2;
                Some(format!("C{}", kind as char))
            }
            b'D' => {
                let kind = self.peek_at(1)?;
                if !matches!(kind, b'0'..=b'5') {
                    return None;
                }
                self.at += 2;
                Some(format!("D{}", kind as char))
            }
            b'a'..=b'z' => {
                let (first, second) = (self.peek()?, self.peek_at(1)?);
                // `cv <type>` (a conversion) and `li` (a literal operator)
                // need more than two letters.
                if !second.is_ascii_lowercase() || (first, second) == (b'c', b'v') {
                    return None;
                }
                if (first, second) == (b'l', b'i') {
                    return None;
                }
                self.at += 2;
                Some(format!("{}{}", first as char, second as char))
            }
            _ => None,
        }
    }

    /// A name's components, written back as one mangled name: a lone
    /// component as itself, more as `N...E`.
    fn joined(components: &[String]) -> String {
        match components {
            [single] => single.clone(),
            many => format!("N{}E", many.concat()),
        }
    }

    /// The components of an expanded name, so a nested name can go on
    /// from a substitution: `N1A1BE` is `1A`, `1B`.
    fn components(expanded: &str) -> Vec<String> {
        let inner = match expanded
            .strip_prefix('N')
            .and_then(|rest| rest.strip_suffix('E'))
        {
            Some(inner) => inner,
            None => return alloc::vec![expanded.into()],
        };
        // Split on source names; anything else (templates, std) stays whole.
        let bytes = inner.as_bytes();
        let mut components = Vec::new();
        let mut at = 0;
        while at < bytes.len() {
            let start = at;
            while at < bytes.len() && bytes[at].is_ascii_digit() {
                at += 1;
            }
            let Some(length) = core::str::from_utf8(&bytes[start..at])
                .ok()
                .and_then(|digits| digits.parse::<usize>().ok())
            else {
                // Not a plain run of source names: keep it whole.
                return alloc::vec![expanded.into()];
            };
            at += length;
            let mut end = at.min(bytes.len());
            // A template's arguments belong to the component before them.
            if bytes.get(end) == Some(&b'I') {
                let mut depth = 0usize;
                while end < bytes.len() {
                    match bytes[end] {
                        b'I' => depth += 1,
                        b'E' => {
                            depth -= 1;
                            if depth == 0 {
                                end += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    end += 1;
                }
            }
            components.push(inner[start..end].into());
            at = end;
        }
        components
    }

    /// The encoding's name: a nested name, whose components but the last
    /// are substitution candidates, or an unscoped one. Answers the last
    /// component and the method's `const`.
    fn function_name(&mut self) -> Option<(String, bool)> {
        if !self.eat(b'N') {
            let name = self.unqualified_name()?;
            if self.peek() == Some(b'I') {
                // A function template: its arguments are no slot's concern.
                self.candidate(&name);
                self.template_arguments()?;
            }
            return Some((name, false));
        }
        let mut constant = false;
        loop {
            match self.peek()? {
                b'r' | b'V' => self.at += 1,
                b'K' => {
                    constant = true;
                    self.at += 1;
                }
                _ => break,
            }
        }
        // A ref-qualifier.
        if matches!(self.peek()?, b'R' | b'O') {
            self.at += 1;
        }
        let mut components: Vec<String> = Vec::new();
        let mut last = None;
        loop {
            match self.peek()? {
                b'E' => {
                    self.at += 1;
                    break;
                }
                b'S' if components.is_empty() && last.is_none() => {
                    let expanded = self.substitution()?;
                    components = Self::components(&expanded);
                }
                b'I' => {
                    // Template arguments for the component just read: the
                    // template's name is a candidate, and the template-id
                    // is the prefix of what follows, or the function.
                    components.push(last.take()?);
                    let template = Self::joined(&components);
                    self.candidate(&template);
                    let arguments = self.template_arguments()?;
                    last = Some(components.pop()? + &arguments);
                }
                _ => {
                    if let Some(previous) = last.take() {
                        components.push(previous);
                        let joined = Self::joined(&components);
                        self.candidate(&joined);
                    }
                    last = Some(self.unqualified_name()?);
                }
            }
        }
        Some((last?, constant))
    }

    /// `<template-args>`: `I`, the arguments, `E`.
    fn template_arguments(&mut self) -> Option<String> {
        if !self.eat(b'I') {
            return None;
        }
        let mut arguments = String::from("I");
        while !self.eat(b'E') {
            match self.peek()? {
                b'L' => {
                    // A literal: its type and value, as written.
                    let start = self.at;
                    self.at += 1;
                    if self.peek()? == b'_' {
                        return None;
                    }
                    self.ty()?;
                    while self.peek()? != b'E' {
                        self.at += 1;
                    }
                    self.at += 1;
                    arguments.push_str(core::str::from_utf8(&self.text[start..self.at]).ok()?);
                }
                b'X' | b'J' | b'T' => return None,
                _ => arguments.push_str(&self.ty()?),
            }
        }
        arguments.push('E');
        Some(arguments)
    }

    /// `<substitution>`: answers what it stands for, adding nothing.
    fn substitution(&mut self) -> Option<String> {
        if !self.eat(b'S') {
            return None;
        }
        let index = match self.peek()? {
            b'_' => {
                self.at += 1;
                0
            }
            b't' => {
                // `St <unqualified-name>`: a name in std, itself a candidate.
                self.at += 1;
                let name = format!("St{}", self.unqualified_name()?);
                self.candidate(&name);
                return Some(name);
            }
            abbreviation @ (b'a' | b'b' | b's' | b'i' | b'o' | b'd') => {
                self.at += 1;
                return Some(format!("S{}", abbreviation as char));
            }
            _ => {
                // A base-36 sequence ID, digits and upper case, then `_`.
                let mut sequence = 0usize;
                loop {
                    let byte = self.peek()?;
                    self.at += 1;
                    let digit = match byte {
                        b'0'..=b'9' => usize::from(byte - b'0'),
                        b'A'..=b'Z' => usize::from(byte - b'A') + 10,
                        b'_' => break,
                        _ => return None,
                    };
                    sequence = sequence.checked_mul(36)?.checked_add(digit)?;
                }
                sequence.checked_add(1)?
            }
        };
        self.substitutions.get(index).cloned()
    }

    /// `<type>`, expanded; every type but a builtin one is a candidate
    /// once read, and a substitution adds nothing.
    fn ty(&mut self) -> Option<String> {
        let byte = self.peek()?;
        match byte {
            b'v' | b'w' | b'b' | b'c' | b'a' | b'h' | b's' | b't' | b'i' | b'j' | b'l' | b'm'
            | b'x' | b'y' | b'n' | b'o' | b'f' | b'd' | b'e' | b'g' | b'z' => {
                self.at += 1;
                Some(String::from(byte as char))
            }
            b'D' => {
                let kind = self.peek_at(1)?;
                if !matches!(
                    kind,
                    b'n' | b'f' | b'd' | b'e' | b'h' | b'i' | b's' | b'u' | b'a' | b'c'
                ) {
                    // Packs, decltype, vectors and the rest.
                    return None;
                }
                self.at += 2;
                Some(format!("D{}", kind as char))
            }
            b'u' => {
                self.at += 1;
                let name = format!("u{}", self.source_name()?);
                self.candidate(&name);
                Some(name)
            }
            b'r' | b'V' | b'K' => {
                let mut qualifiers = String::new();
                while let Some(qualifier @ (b'r' | b'V' | b'K')) = self.peek() {
                    qualifiers.push(qualifier as char);
                    self.at += 1;
                }
                let qualified = qualifiers + &self.ty()?;
                self.candidate(&qualified);
                Some(qualified)
            }
            b'P' | b'R' | b'O' => {
                self.at += 1;
                let pointer = format!("{}{}", byte as char, self.ty()?);
                self.candidate(&pointer);
                Some(pointer)
            }
            b'F' => {
                self.at += 1;
                let mut function = String::from("F");
                if self.eat(b'Y') {
                    function.push('Y');
                }
                loop {
                    match self.peek()? {
                        b'E' => {
                            self.at += 1;
                            break;
                        }
                        qualifier @ (b'R' | b'O') if self.peek_at(1) == Some(b'E') => {
                            function.push(qualifier as char);
                            self.at += 1;
                        }
                        _ => function.push_str(&self.ty()?),
                    }
                }
                function.push('E');
                self.candidate(&function);
                Some(function)
            }
            b'A' => {
                self.at += 1;
                let length = self.number()?;
                if !self.eat(b'_') {
                    return None;
                }
                let array = format!("A{length}_{}", self.ty()?);
                self.candidate(&array);
                Some(array)
            }
            b'M' => {
                self.at += 1;
                let class = self.ty()?;
                let member = self.ty()?;
                let pointer = format!("M{class}{member}");
                self.candidate(&pointer);
                Some(pointer)
            }
            b'N' => self.nested_type(),
            b'0'..=b'9' => {
                let name = self.source_name()?;
                self.candidate(&name);
                if self.peek() == Some(b'I') {
                    let templated = name + &self.template_arguments()?;
                    self.candidate(&templated);
                    return Some(templated);
                }
                Some(name)
            }
            b'S' => {
                let expanded = self.substitution()?;
                if self.peek() == Some(b'I') {
                    let templated = expanded + &self.template_arguments()?;
                    self.candidate(&templated);
                    return Some(templated);
                }
                Some(expanded)
            }
            _ => None,
        }
    }

    /// A nested class name, `N ... E`: every prefix is a candidate, the
    /// whole name included.
    fn nested_type(&mut self) -> Option<String> {
        if !self.eat(b'N') {
            return None;
        }
        let mut components: Vec<String> = Vec::new();
        loop {
            match self.peek()? {
                b'E' => {
                    self.at += 1;
                    break;
                }
                b'S' if components.is_empty() => {
                    let expanded = self.substitution()?;
                    components = Self::components(&expanded);
                }
                b'I' => {
                    let arguments = self.template_arguments()?;
                    let templated = components.pop()? + &arguments;
                    components.push(templated);
                    let joined = Self::joined(&components);
                    self.candidate(&joined);
                }
                _ => {
                    components.push(self.unqualified_name()?);
                    let joined = Self::joined(&components);
                    self.candidate(&joined);
                }
            }
        }
        if components.is_empty() {
            return None;
        }
        Some(Self::joined(&components))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn parameters(symbol: &str) -> Vec<String> {
        signature(symbol)
            .unwrap_or_else(|| panic!("{symbol}"))
            .parameters
    }

    #[test]
    fn an_override_and_the_method_it_replaces_compare_equal() {
        let kernel = signature("__ZN9IOService5startEPS_").unwrap();
        let kext = signature("__ZN15StelaVirtIOMMIO5startEP9IOService").unwrap();
        assert_eq!(kernel, kext);
        assert_eq!(kernel.name, "5start");
        assert_eq!(kernel.parameters, ["P9IOService"]);
        assert!(!kernel.constant);
        // A different method, or the same name over other parameters, differs.
        assert_ne!(
            signature("__ZN9IOService4stopEPS_").unwrap(),
            kernel,
            "another name"
        );
        assert_ne!(
            signature("__ZN15StelaVirtIOMMIO5startEP8OSObject").unwrap(),
            kernel
        );
    }

    #[test]
    fn substitutions_expand_in_order_of_appearance() {
        // S_ is the class, S0_ the first pointer type.
        assert_eq!(
            parameters("__ZN18IOSerialStreamSync12dequeueEventEPjS0_b"),
            ["Pj", "Pj", "b"]
        );
        assert_eq!(
            parameters("__ZN18IOSerialStreamSync11dequeueDataEPhjPjj"),
            ["Ph", "j", "Pj", "j"]
        );
        // const T*: T is S0_, const T S1_, const T* S2_; void* S3_.
        assert_eq!(
            parameters("__ZN9IOService20callPlatformFunctionEPK8OSSymbolbPvS3_S3_S3_"),
            ["PK8OSSymbol", "b", "Pv", "Pv", "Pv", "Pv"]
        );
        let equal = signature("__ZNK15OSMetaClassBase9isEqualToEPKS_").unwrap();
        assert!(equal.constant);
        assert_eq!(equal.parameters, ["PK15OSMetaClassBase"]);
        // A function pointer and the substitutions inside it.
        assert_eq!(
            parameters("__ZN9IOService17registerInterruptEiP8OSObjectPFvS1_PvPS_iES2_"),
            ["i", "P8OSObject", "PFvP8OSObjectPvP9IOServiceiE", "Pv"]
        );
        assert_eq!(parameters("__ZN9IOService8DispatchE5IORPC"), ["5IORPC"]);
        assert_eq!(
            parameters("__ZN18IOSerialStreamSync4initEP12OSDictionaryPv"),
            ["P12OSDictionary", "Pv"]
        );
    }

    #[test]
    fn nested_class_names_and_their_substitutions() {
        // The function's own name is no candidate, its prefixes are.
        let meta = signature("__ZN18IOSerialDriverSync9MetaClassC2Ev").unwrap();
        assert_eq!((meta.name.as_str(), meta.parameters.len()), ("C2", 0));
        assert_eq!(parameters("__ZN1A1B1fEPS0_"), ["PN1A1BE"]);
        assert_eq!(parameters("__ZN1A1B1fEPS_"), ["P1A"]);
        let alloc = signature("__ZNK18IOSerialDriverSync9MetaClass5allocEv").unwrap();
        assert!(alloc.constant && alloc.name == "5alloc");
        // A nested type parameter, then a substitution of it.
        assert_eq!(parameters("__ZN1A1fEN1B1CEPS1_"), ["N1B1CE", "PN1B1CE"]);
        // A templated parameter.
        assert_eq!(parameters("__ZN1A1fE1TIiEPS1_"), ["1TIiE", "P1TIiE"]);
        assert_eq!(signature("__ZN1AD1Ev").unwrap().name, "D1", "a destructor");
        assert_eq!(signature("__ZN1AnwEm").unwrap().name, "nw");
    }

    #[test]
    fn what_it_does_not_cover_has_no_signature() {
        for symbol in [
            "___cxa_pure_virtual",
            "_IOLog",
            "__ZTV9IOService",
            // A template parameter, a pack, decltype.
            "__ZN1A1fET_",
            "__ZN1A1fEDpT_",
            "__ZN1A1fEDtfp_E",
            // Truncated.
            "__ZN9IOService5startE",
            "__ZN9IOService5startEP",
            "__ZN9IOService5star",
        ] {
            assert_eq!(signature(symbol), None, "{symbol}");
        }
        assert_eq!(
            signature("__ZN9IOService4freeEv").unwrap().parameters,
            vec![] as Vec<String>
        );
    }
}

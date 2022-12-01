//! Simple implementation of space which uses in-memory vector of atoms as
//! an underlying storage.

use crate::*;
use crate::atom::*;
use crate::atom::matcher::{Bindings, Unifications, match_atoms};
use crate::atom::subexpr::split_expr;
use crate::matcher::MatchResultIter;
use crate::common::collections::ListMap;

use std::fmt::{Display, Debug};
use std::rc::{Rc, Weak};
use std::cell::RefCell;
use std::collections::HashSet;

// Grounding space

#[derive(PartialEq, Eq, Clone, Debug)]
enum IndexKey {
    Symbol(SymbolAtom),
    Wildcard,
    ExpressionBegin(ExpressionAtom, usize),
    ExpressionEnd,
}

impl IndexKey {
    fn keys_from_atom(atom: &Atom) -> Vec<IndexKey> {
        match atom {
            Atom::Symbol(sym) => vec![IndexKey::Symbol(sym.clone())],
            Atom::Expression(expr) => {
                let mut keys = Vec::new();
                let mut expr_len = 0usize;

                expr_len += 1;
                keys.push(IndexKey::ExpressionEnd);

                for child in expr.children().iter().rev() {
                    let mut children = IndexKey::keys_from_atom(child);
                    expr_len += children.len();
                    keys.append(&mut children);
                }

                keys.push(IndexKey::ExpressionBegin(expr.clone(), expr_len));
                keys
            },
            _ => vec![IndexKey::Wildcard],
        }
    }
}

#[derive(Clone)]
struct IndexTree<T> {
    // FIXME: improve performance using HashMap for symbols and Vec for expressions
    next: ListMap<IndexKey, Box<IndexTree<T>>>,
    leaf: Vec<T>,
}

macro_rules! walk_index_tree {
    ( $IndexTreeIter:ident, {$( $mut_:tt )?}, $raw_mut:tt ) => {
        struct $IndexTreeIter<'a, T, F>
            where F: Fn(&'a $( $mut_ )? IndexTree<T>, IndexKey, Vec<IndexKey>, &mut dyn FnMut(*$raw_mut IndexTree<T>, Vec<IndexKey>))
        {
            queue: Vec<(* $raw_mut IndexTree<T>, Vec<IndexKey>)>,
            next_op: F,
            _marker: std::marker::PhantomData<&'a $( $mut_ )? IndexTree<T>>,
        }

        impl<'a, T, F> $IndexTreeIter<'a, T, F>
            where F: Fn(&'a $( $mut_ )? IndexTree<T>, IndexKey, Vec<IndexKey>, &mut dyn FnMut(*$raw_mut IndexTree<T>, Vec<IndexKey>))
        {
            fn new(idx: &'a $( $mut_ )? IndexTree<T>, atom: &Atom, next_op: F) -> Self {
                let idx: * $raw_mut IndexTree<T> = idx;
                let mut queue = Vec::new();

                queue.push((idx, IndexKey::keys_from_atom(&atom)));

                Self{ queue: queue, next_op, _marker: std::marker::PhantomData }
            }

            fn call_next(&mut self, idx: * $raw_mut IndexTree<T>, key: IndexKey, keys: Vec<IndexKey>) {
                let queue = &mut self.queue;
                (self.next_op)(unsafe{ & $( $mut_ )? *idx}, key, keys, &mut |index, keys| queue.push((index, keys)));
            }
        }

        impl<'a, T, F> Iterator for $IndexTreeIter<'a, T, F>
            where F: Fn(&'a $( $mut_ )? IndexTree<T>, IndexKey, Vec<IndexKey>, &mut dyn FnMut(*$raw_mut IndexTree<T>, Vec<IndexKey>))
        {
            type Item = &'a $( $mut_ )? IndexTree<T>;

            fn next(&mut self) -> Option<Self::Item> {
                while let Some((idx, mut keys)) = self.queue.pop() {
                    match keys.pop() {
                        None => return Some(unsafe{ & $( $mut_ )? *idx }),
                        Some(key) => self.call_next(idx, key, keys),
                    }
                }
                None
            }
        }
    }
}

walk_index_tree!(IndexTreeIterMut, { mut }, mut);
walk_index_tree!(IndexTreeIter, { /* no mut */ }, const);

impl<T: PartialEq + Clone> IndexTree<T> {

    fn new() -> Self {
        Self{ next: ListMap::new(), leaf: Vec::new() }
    }

    fn next_or_insert<'a>(&'a mut self, key: IndexKey, keys: Vec<IndexKey>,
            callback: &mut dyn FnMut(*mut IndexTree<T>, Vec<IndexKey>)) {
        // FIXME: make faster and remove clone
        self.next.entry(key.clone()).or_insert(Box::new(IndexTree::new()));
        let idx = self.next.get_mut(&key).unwrap();
        if let IndexKey::ExpressionBegin(_, expr_len) = key {
            let len = keys.len() - expr_len;
            let tail = &keys.as_slice()[..len];
            callback(idx.as_mut(), tail.to_vec());
        }
        callback(idx.as_mut(), keys)
    }

    fn remove_value(&mut self, value: &T) -> bool {
        match self.leaf.iter().position(|other| *other == *value) {
            Some(position) => {
                self.leaf.remove(position);
                true
            },
            None => false,
        }
    }

    fn next<'a>(&'a self, key: IndexKey, keys: Vec<IndexKey>,
            callback: &mut dyn FnMut(*const IndexTree<T>, Vec<IndexKey>)) {
        match key {
            IndexKey::Symbol(_) => {
                self.next.get(&key).map_or((), |idx| callback(idx.as_ref(), keys.clone()));
                self.next.get(&IndexKey::Wildcard).map_or((), |idx| callback(idx.as_ref(), keys));
            },
            IndexKey::ExpressionEnd => self.next.get(&key).map_or((), |idx| callback(idx.as_ref(), keys)),
            IndexKey::ExpressionBegin(_, expr_len) => {
                let len = keys.len() - expr_len;
                let tail = &keys.as_slice()[..len];
                self.next.get(&IndexKey::Wildcard).map_or((), |idx| callback(idx.as_ref(), tail.to_vec()));
                match self.next.get(&key) {
                    Some(idx) => callback(idx.as_ref(), keys),
                    None => {
                        self.next.iter().for_each(|(key, idx)| {
                            if let IndexKey::ExpressionBegin(_, _) = *key {
                                callback(idx.as_ref(), keys.clone());
                            }
                        });
                    }
                }
            },
            IndexKey::Wildcard =>
                self.next.iter().for_each(|(key, idx)| {
                    if *key != IndexKey::ExpressionEnd {
                        callback(idx.as_ref(), keys.clone())
                    }
                }),
        }
    }
    
    fn add(&mut self, key: &Atom, value: T) {
        IndexTreeIterMut::new(self, key, |idx, key, keys, callback| {
            idx.next_or_insert(key, keys, callback)
        }).for_each(|idx| idx.leaf.push(value.clone()));
    }

    fn remove(&mut self, key: &Atom, value: &T) -> bool {
        IndexTreeIterMut::new(self, &key, |idx, key, keys, callback| {
            // FIXME: remove second expression path
            idx.next.get_mut(&key).map_or({}, |idx| callback(idx.as_mut(), keys))
        }).map(|idx| idx.remove_value(value)).fold(false, |a, b| a | b)
    }

    // FIXME: actually we can call match on Atom instead of returning it
    fn get(&self, pattern: &Atom) -> impl Iterator<Item=&T> {
        IndexTreeIter::new(self, &pattern, |idx, key, keys, callback| {
            idx.next(key, keys, callback)
        }).flat_map(|idx| idx.leaf.as_slice().iter())
    }
}

/// Symbol to concatenate queries to space.
pub const COMMA_SYMBOL : Atom = sym!(",");

/// Contains information about space modification event.
#[derive(Clone, Debug, PartialEq)]
pub enum SpaceEvent {
    /// Atom is added into a space.
    Add(Atom),
    /// Atom is removed from space.
    Remove(Atom),
    /// First atom is replaced by the second one.
    Replace(Atom, Atom),
}

/// Space modification event observer trait.
///
/// # Examples
///
/// ```
/// use hyperon::sym;
/// use hyperon::space::grounding::*;
/// use std::rc::Rc;
/// use std::cell::RefCell;
///
/// struct MyObserver {
///     events: Vec<SpaceEvent>
/// }
///
/// impl SpaceObserver for MyObserver {
///     fn notify(&mut self, event: &SpaceEvent) {
///         self.events.push(event.clone());
///     }
/// }
///
/// let mut space = GroundingSpace::new();
/// let observer = Rc::new(RefCell::new(MyObserver{ events: Vec::new() }));
///
/// space.register_observer(Rc::clone(&observer));
/// space.add(sym!("A"));
/// space.replace(&sym!("A"), sym!("B"));
/// space.remove(&sym!("B"));
///
/// assert_eq!(observer.borrow().events, vec![SpaceEvent::Add(sym!("A")),
///     SpaceEvent::Replace(sym!("A"), sym!("B")),
///     SpaceEvent::Remove(sym!("B"))]);
/// ```
pub trait SpaceObserver {
    /// Notifies about space modification.
    fn notify(&mut self, event: &SpaceEvent);
}

/// In-memory space which can contain grounded atoms.
// TODO: Clone is required by C API
#[derive(Clone)]
pub struct GroundingSpace {
    content: Vec<Atom>,
    observers: RefCell<Vec<Weak<RefCell<dyn SpaceObserver>>>>,
}

impl GroundingSpace {

    /// Constructs new empty space.
    pub fn new() -> Self {
        Self {
            content: Vec::new(),
            observers: RefCell::new(Vec::new()),
        }
    }

    /// Constructs space from vector of atoms.
    pub fn from_vec(atoms: Vec<Atom>) -> Self {
        Self{
            content: atoms,
            observers: RefCell::new(Vec::new()),
        }
    }

    /// Registers space modifications `observer`. Observer is automatically
    /// deregistered when `Rc` counter reaches zero. See [SpaceObserver] for
    /// examples.
    pub fn register_observer<T>(&self, observer: Rc<RefCell<T>>)
        where T: SpaceObserver + 'static
    {
        self.observers.borrow_mut().push(Rc::downgrade(&observer) as Weak<RefCell<dyn SpaceObserver>>);
    }

    /// Notifies registered observers about space modification `event`.
    fn notify(&self, event: &SpaceEvent) {
        let mut cleanup = false;
        for observer in self.observers.borrow_mut().iter() {
            if let Some(observer) = observer.upgrade() {
                observer.borrow_mut().notify(event);
            } else {
                cleanup = true;
            }
        }
        if cleanup {
            self.observers.borrow_mut().retain(|w| w.strong_count() > 0);
        }
    }

    /// Adds `atom` into space.
    ///
    /// # Examples
    ///
    /// ```
    /// use hyperon::sym;
    /// use hyperon::space::grounding::GroundingSpace;
    ///
    /// let mut space = GroundingSpace::from_vec(vec![sym!("A")]);
    /// 
    /// space.add(sym!("B"));
    ///
    /// assert_eq!(space.into_vec(), vec![sym!("A"), sym!("B")]);
    /// ```
    pub fn add(&mut self, atom: Atom) {
        self.content.push(atom.clone());
        self.notify(&SpaceEvent::Add(atom));
    }

    /// Removes `atom` from space. Returns true if atom was found and removed,
    /// and false otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// use hyperon::sym;
    /// use hyperon::space::grounding::GroundingSpace;
    ///
    /// let mut space = GroundingSpace::from_vec(vec![sym!("A")]);
    /// 
    /// space.remove(&sym!("A"));
    ///
    /// assert!(space.into_vec().is_empty());
    /// ```
    pub fn remove(&mut self, atom: &Atom) -> bool {
        let position = self.content.iter().position(|other| other == atom);
        match position {
            Some(position) => {
                self.content.remove(position);
                self.notify(&SpaceEvent::Remove(atom.clone()));
                true
            },
            None => false, 
        }
    }

    /// Replaces `from` atom to `to` atom inside space. Doesn't add `to` when
    /// `from` is not found. Returns true if atom was found and replaced, and
    /// false otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// use hyperon::sym;
    /// use hyperon::space::grounding::GroundingSpace;
    ///
    /// let mut space = GroundingSpace::from_vec(vec![sym!("A")]);
    /// 
    /// space.replace(&sym!("A"), sym!("B"));
    ///
    /// assert_eq!(space.into_vec(), vec![sym!("B")]);
    /// ```
    pub fn replace(&mut self, from: &Atom, to: Atom) -> bool {
        let position = self.content.iter().position(|other| other == from);
        match position {
            Some(position) => {
                self.content.as_mut_slice()[position] = to.clone();
                self.notify(&SpaceEvent::Replace(from.clone(), to));
                true
            },
            None => false, 
        }
    }

    /// Executes `query` on the space and returns variable bindings found.
    /// Query may include sub-queries glued by [COMMA_SYMBOL] symbol. Number
    /// of results is equal to the length of the `Vec<Bindings>` returned.
    /// Each [Bindings] instance represents single result.
    ///
    /// # Examples
    ///
    /// ```
    /// use hyperon::{expr, bind, sym};
    /// use hyperon::space::grounding::GroundingSpace;
    ///
    /// let space = GroundingSpace::from_vec(vec![expr!("A" "B"), expr!("B" "C")]);
    /// let query = expr!("," ("A" x) (x "C"));
    ///
    /// let result = space.query(&query);
    ///
    /// assert_eq!(result, vec![bind!{x: sym!("B")}]);
    /// ```
    pub fn query(&self, query: &Atom) -> Vec<Bindings> {
        match split_expr(query) {
            // Cannot match with COMMA_SYMBOL here, because Rust allows
            // it only when Atom has PartialEq and Eq derived.
            Some((sym @ Atom::Symbol(_), args)) if *sym == COMMA_SYMBOL => {
                let vars = collect_variables(&query);
                let mut result = args.fold(vec![bind!{}],
                    |mut acc, query| {
                        let result = if acc.is_empty() {
                            acc
                        } else {
                            acc.drain(0..).flat_map(|prev| -> Vec<Bindings> {
                                let query = matcher::apply_bindings_to_atom(&query, &prev);
                                let mut res = self.query(&query);
                                res.drain(0..)
                                    .map(|next| Bindings::merge(&prev, &next))
                                    .filter(Option::is_some).map(Option::unwrap)
                                    .map(|next| matcher::apply_bindings_to_bindings(&next, &next)
                                        .expect("Self consistent bindings are expected"))
                                    .collect()
                            }).collect()
                        };
                        log::debug!("query: current result: {:?}", result);
                        result
                    });
                result.iter_mut().for_each(|bindings| bindings.filter(|k, _v| vars.contains(k)));
                result
            },
            _ => self.single_query(query),
        }
    }

    /// Executes simple `query` without sub-queries on the space.
    fn single_query(&self, query: &Atom) -> Vec<Bindings> {
        log::debug!("single_query: query: {}", query);
        let mut result = Vec::new();
        for next in &self.content {
            let next = make_variables_unique(next);
            log::trace!("single_query: match next: {}", next);
            for bindings in match_atoms(&next, query) {
                log::trace!("single_query: push result: {}", bindings);
                result.push(bindings);
            }
        }
        log::debug!("single_query: result: {:?}", result);
        result
    }

    /// Executes `pattern` query on the space and for each result substitutes
    /// variables in `template` by the values from `pattern`. Returns results
    /// of the substitution.
    ///
    /// # Examples
    ///
    /// ```
    /// use hyperon::expr;
    /// use hyperon::space::grounding::GroundingSpace;
    ///
    /// let space = GroundingSpace::from_vec(vec![expr!("A" "B"), expr!("A" "C")]);
    ///
    /// let result = space.subst(&expr!("A" x), &expr!("D" x));
    ///
    /// assert_eq!(result, vec![expr!("D" "B"), expr!("D" "C")]);
    /// ```
    pub fn subst(&self, pattern: &Atom, template: &Atom) -> Vec<Atom> {
        self.query(pattern).drain(0..)
            .map(| bindings | matcher::apply_bindings_to_atom(template, &bindings))
            .collect()
    }

    // TODO: for now we have separate methods query() and unify() but
    // they probably can be merged. One way of doing it is designating
    // in the query which part of query should be unified and which matched.
    // For example for the typical query in a form (= (+ a b) $X) the
    // (= (...) $X) level should not be unified otherwise we will recursively
    // infer that we need calculating (+ a b) again which is equal to original
    // query. Another option is designating this in the data itself.
    // After combining match and unification we could leave only single
    // universal method.
    #[doc(hidden)]
    pub fn unify(&self, pattern: &Atom) -> Vec<(Bindings, Unifications)> {
        log::debug!("unify: pattern: {}", pattern);
        let mut result = Vec::new();
        for next in &self.content {
            match matcher::unify_atoms(next, pattern) {
                Some(res) => {
                    let bindings = matcher::apply_bindings_to_bindings(&res.data_bindings, &res.pattern_bindings);
                    if let Ok(bindings) = bindings {
                        // TODO: implement Display for bindings
                        log::debug!("unify: push result: {}, bindings: {:?}", next, bindings);
                        result.push((bindings, res.unifications));
                    }
                },
                None => continue,
            }
        }
        result
    }

    /// Returns the reference to the vector of the atoms in the space.
    pub fn content(&self) -> &Vec<Atom> {
        &self.content
    }

    /// Converts space into a vector of atoms.
    pub fn into_vec(self) -> Vec<Atom> {
        self.content.clone()
    }
}

impl PartialEq for GroundingSpace {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
    }
}

impl Debug for GroundingSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GroundingSpace")
    }
}

impl Display for GroundingSpace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "GroundingSpace")
    }
}

impl Grounded for GroundingSpace {
    fn type_(&self) -> Atom {
        rust_type_atom::<GroundingSpace>()
    }

    fn match_(&self, other: &Atom) -> MatchResultIter {
        Box::new(self.query(other).into_iter())
    }

    fn execute(&self, _args: &mut Vec<Atom>) -> Result<Vec<Atom>, ExecError> {
        execute_not_executable(self)
    }
}

fn collect_variables(atom: &Atom) -> HashSet<VariableAtom> {
    fn recursion(atom: &Atom, vars: &mut HashSet<VariableAtom>) {
        match atom {
            Atom::Variable(var) => { vars.insert(var.clone()); },
            Atom::Expression(expr) => {
                expr.children().iter().for_each(|child| recursion(child, vars));
            }
            _ => {},
        }
    }
    let mut vars = HashSet::new();
    recursion(atom, &mut vars);
    vars
}


#[cfg(test)]
mod test {
    use super::*;

    struct SpaceEventCollector {
        events: Vec<SpaceEvent>,
    }

    impl SpaceEventCollector {
        fn new() -> Self {
            Self{ events: Vec::new() }
        }
    }

    impl SpaceObserver for SpaceEventCollector {
        fn notify(&mut self, event: &SpaceEvent) {
            self.events.push(event.clone());
        }
    }

    #[test]
    fn add_atom() {
        let mut space = GroundingSpace::new();
        let observer = Rc::new(RefCell::new(SpaceEventCollector::new()));
        space.register_observer(Rc::clone(&observer));

        space.add(expr!("a"));
        space.add(expr!("b"));
        space.add(expr!("c"));

        assert_eq!(*space.content, vec![expr!("a"), expr!("b"), expr!("c")]);
        assert_eq!(observer.borrow().events, vec![SpaceEvent::Add(sym!("a")),
            SpaceEvent::Add(sym!("b")), SpaceEvent::Add(sym!("c"))]);
    }

    #[test]
    fn remove_atom() {
        let mut space = GroundingSpace::new();
        let observer = Rc::new(RefCell::new(SpaceEventCollector::new()));
        space.register_observer(Rc::clone(&observer));

        space.add(expr!("a"));
        space.add(expr!("b"));
        space.add(expr!("c"));
        assert_eq!(space.remove(&expr!("b")), true);

        assert_eq!(*space.content, vec![expr!("a"), expr!("c")]);
        assert_eq!(observer.borrow().events, vec![SpaceEvent::Add(sym!("a")),
            SpaceEvent::Add(sym!("b")), SpaceEvent::Add(sym!("c")),
            SpaceEvent::Remove(sym!("b"))]);
    }

    #[test]
    fn remove_atom_not_found() {
        let mut space = GroundingSpace::new();
        let observer = Rc::new(RefCell::new(SpaceEventCollector::new()));
        space.register_observer(Rc::clone(&observer));

        space.add(expr!("a"));
        assert_eq!(space.remove(&expr!("b")), false);

        assert_eq!(*space.content, vec![expr!("a")]);
        assert_eq!(observer.borrow().events, vec![SpaceEvent::Add(sym!("a"))]);
    }

    #[test]
    fn replace_atom() {
        let mut space = GroundingSpace::new();
        let observer = Rc::new(RefCell::new(SpaceEventCollector::new()));
        space.register_observer(Rc::clone(&observer));

        space.add(expr!("a"));
        space.add(expr!("b"));
        space.add(expr!("c"));
        assert_eq!(space.replace(&expr!("b"), expr!("d")), true);

        assert_eq!(*space.content, vec![expr!("a"), expr!("d"), expr!("c")]);
        assert_eq!(observer.borrow().events, vec![SpaceEvent::Add(sym!("a")),
            SpaceEvent::Add(sym!("b")), SpaceEvent::Add(sym!("c")),
            SpaceEvent::Replace(sym!("b"), sym!("d"))]);
    }

    #[test]
    fn replace_atom_not_found() {
        let mut space = GroundingSpace::new();
        let observer = Rc::new(RefCell::new(SpaceEventCollector::new()));
        space.register_observer(Rc::clone(&observer));

        space.add(expr!("a"));
        assert_eq!(space.replace(&expr!("b"), expr!("d")), false);

        assert_eq!(*space.content, vec![expr!("a")]);
        assert_eq!(observer.borrow().events, vec![SpaceEvent::Add(sym!("a"))]);
    }

    #[test]
    fn mut_cloned_atomspace() {
        let mut first = GroundingSpace::new();
        let mut second = first.clone(); 

        first.add(expr!("b"));
        second.add(expr!("d"));

        assert_eq!(*first.content, vec![expr!("b")]);
        assert_eq!(*second.content, vec![expr!("d")]);
    }

    #[test]
    fn test_match_symbol() {
        let mut space = GroundingSpace::new();
        space.add(expr!("foo"));
        assert_eq!(space.query(&expr!("foo")), vec![bind!{}]);
    }

    #[test]
    fn test_match_variable() {
        let mut space = GroundingSpace::new();
        space.add(expr!("foo"));
        assert_eq!(space.query(&expr!(x)), vec![bind!{x: expr!("foo")}]);
    }

    #[test]
    fn test_match_expression() {
        let mut space = GroundingSpace::new();
        space.add(expr!("+" "a" ("*" "b" "c")));
        assert_eq!(space.query(&expr!("+" "a" ("*" "b" "c"))), vec![bind!{}]);
    }

    #[test]
    fn test_match_expression_with_variables() {
        let mut space = GroundingSpace::new();
        space.add(expr!("+" "A" ("*" "B" "C")));
        assert_eq!(space.query(&expr!("+" a ("*" b c))),
        vec![bind!{a: expr!("A"), b: expr!("B"), c: expr!("C") }]);
    }

    #[test]
    fn test_match_different_value_for_variable() {
        let mut space = GroundingSpace::new();
        space.add(expr!("+" "A" ("*" "B" "C")));
        assert_eq!(space.query(&expr!("+" a ("*" a c))), vec![]);
    }

    fn get_var<'a>(bindings: &'a Bindings, name: &str) -> &'a Atom {
        bindings.get(&VariableAtom::new(name)).unwrap()
    }

    #[test]
    fn test_match_query_variable_has_priority() {
        let mut space = GroundingSpace::new();
        space.add(expr!("equals" x x));
        
        let result = space.query(&expr!("equals" y z));
        assert_eq!(result.len(), 1);
        assert!(matches!(get_var(&result[0], "y"), Atom::Variable(_)));
        assert!(matches!(get_var(&result[0], "z"), Atom::Variable(_)));
    }

    #[test]
    fn test_match_query_variable_via_data_variable() {
        let mut space = GroundingSpace::new();
        space.add(expr!(x x));
        assert_eq!(space.query(&expr!(y (z))), vec![bind!{y: expr!((z))}]);
    }

    #[test]
    fn test_match_if_then_with_x() {
        let mut space = GroundingSpace::new();
        space.add(expr!("=" ("if" "True" then) then));
        assert_eq!(space.query(&expr!("=" ("if" "True" "42") X)),
        vec![bind!{X: expr!("42")}]);
    }

    #[test]
    fn test_match_combined_query() {
        let mut space = GroundingSpace::new();
        space.add(expr!("posesses" "Sam" "baloon"));
        space.add(expr!("likes" "Sam" ("blue" "stuff")));
        space.add(expr!("has-color" "baloon" "blue"));

        let result = space.query(&expr!("," ("posesses" "Sam" object)
        ("likes" "Sam" (color "stuff"))
        ("has-color" object color)));
        assert_eq!(result, vec![bind!{object: expr!("baloon"), color: expr!("blue")}]);
    }

    #[test]
    fn test_unify_variables_inside_conjunction_query() {
        let mut space = GroundingSpace::new();
        space.add(expr!("lst1" ("Cons" "a1" ("Cons" "b2" "b3"))));
        space.add(expr!("lst2" ("Cons" "a2" ("Cons" "b3" "b4"))));
        space.add(expr!("Concat" x1 x2 x3));

        let result = space.subst(
            &expr!("," ("lst1" l1) ("lst2" l2) ("Concat" l1 "a2" "a3")),
            &expr!(l1));
        assert_eq!(result, vec![expr!("Cons" "a1" ("Cons" "b2" "b3"))]);
    }

    #[test]
    fn test_type_check_in_query() {
        let mut space = GroundingSpace::new();
        space.add(expr!(":" "Human" "Type"));
        space.add(expr!(":" "Socrates" "Human"));
        space.add(expr!("Cons" "Socrates" "Nil"));

        let result = space.query(&expr!("," (":" h "Human") ("Cons" h t)));
        assert_eq!(result, vec![bind!{h: expr!("Socrates"), t: expr!("Nil")}]);
    }

    #[test]
    fn cleanup_observer() {
        let mut space = GroundingSpace::new();
        {
            let observer = Rc::new(RefCell::new(SpaceEventCollector::new()));
            space.register_observer(Rc::clone(&observer));
            assert_eq!(space.observers.borrow().len(), 1);
        }

        space.add(expr!("a"));

        assert_eq!(*space.content, vec![expr!("a")]);
        assert_eq!(space.observers.borrow().len(), 0);
    }

    #[test]
    fn complex_query_applying_bindings_to_next_pattern() {
        let mut space = GroundingSpace::new();
        space.add(expr!(":=" ("sum" a b) ("+" a b)));
        space.add(expr!(":=" "a" {4}));

        let result = space.query(&expr!("," (":=" "a" b) (":=" ("sum" {3} b) W)));

        assert_eq!(result, vec![bind!{b: expr!({4}), W: expr!("+" {3} {4})}]);
    }

    #[test]
    fn complex_query_chain_of_bindings() {
        let mut space = GroundingSpace::new();
        space.add(expr!("implies" ("B" x) ("C" x)));
        space.add(expr!("implies" ("A" x) ("B" x)));
        space.add(expr!("A" "Sam"));

        let result = space.query(&expr!("," ("implies" ("B" x) z) ("implies" ("A" x) y) ("A" x)));
        assert_eq!(result, vec![bind!{x: sym!("Sam"), y: expr!("B" "Sam"), z: expr!("C" "Sam")}]);
    }

    #[test]
    fn test_custom_match_with_space() {
        let space = GroundingSpace::from_vec(vec![
            expr!("A" {1} x "a"),
            expr!("B" {1} x "b"),
            expr!("A" {2} x "c"),
        ]);
        let result: Vec<Bindings> = match_atoms(&Atom::gnd(space), &expr!("A" {1} x x)).collect();
        assert_eq!(result, vec![bind!{x: sym!("a")}]);
    }

    trait IntoVec<T: Ord> {
        fn to_vec(self) -> Vec<T>;
    }

    impl<'a, T: 'a + Ord + Clone, I: Iterator<Item=&'a T>> IntoVec<T> for I {
        fn to_vec(self) -> Vec<T> {
            let mut vec: Vec<T> = self.cloned().collect();
            vec.sort();
            vec
        }
    }

    #[test]
    fn index_tree_add_atom_basic() {
        let mut index = IndexTree::new();
        index.add(&Atom::sym("A"), 1);
        index.add(&Atom::value(1), 2);
        index.add(&Atom::var("a"), 3);
        index.add(&expr!("A" "B"), 4);

        // TODO: index doesn't match grounded atoms yet, it considers them as wildcards
        // as matching can be redefined for them
        assert_eq!(index.get(&Atom::sym("A")).to_vec(), vec![1, 2, 3]);
        assert_eq!(index.get(&Atom::sym("B")).to_vec(), vec![2, 3]);

        assert_eq!(index.get(&Atom::value(1)).to_vec(), vec![1, 2, 3, 4]);
        assert_eq!(index.get(&Atom::value(2)).to_vec(), vec![1, 2, 3, 4]);

        assert_eq!(index.get(&expr!("A" "B")).to_vec(), vec![2, 3, 4]);
        assert_eq!(index.get(&expr!("A" "C")).to_vec(), vec![2, 3]);
    }

    #[test]
    fn index_tree_add_atom_expr() {
        let mut index = IndexTree::new();
        index.add(&expr!(("A") "B"), 1);
        index.add(&expr!(a "C"), 2);

        assert_eq!(index.get(&expr!(a "B")).to_vec(), vec![1]);
        assert_eq!(index.get(&expr!(("A") "C")).to_vec(), vec![2]);
    }
}

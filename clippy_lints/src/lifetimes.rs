use crate::reexport::*;
use rustc::lint::*;
use rustc::hir::def::Def;
use rustc::hir::*;
use rustc::hir::intravisit::*;
use std::collections::{HashMap, HashSet};
use syntax::codemap::Span;
use crate::utils::{in_external_macro, last_path_segment, span_lint};
use syntax::symbol::keywords;

/// **What it does:** Checks for lifetime annotations which can be removed by
/// relying on lifetime elision.
///
/// **Why is this bad?** The additional lifetimes make the code look more
/// complicated, while there is nothing out of the ordinary going on. Removing
/// them leads to more readable code.
///
/// **Known problems:** Potential false negatives: we bail out if the function
/// has a `where` clause where lifetimes are mentioned.
///
/// **Example:**
/// ```rust
/// fn in_and_out<'a>(x: &'a u8, y: u8) -> &'a u8 { x }
/// ```
declare_clippy_lint! {
    pub NEEDLESS_LIFETIMES,
    complexity,
    "using explicit lifetimes for references in function arguments when elision rules \
     would allow omitting them"
}

/// **What it does:** Checks for lifetimes in generics that are never used
/// anywhere else.
///
/// **Why is this bad?** The additional lifetimes make the code look more
/// complicated, while there is nothing out of the ordinary going on. Removing
/// them leads to more readable code.
///
/// **Known problems:** None.
///
/// **Example:**
/// ```rust
/// fn unused_lifetime<'a>(x: u8) { .. }
/// ```
declare_clippy_lint! {
    pub EXTRA_UNUSED_LIFETIMES,
    complexity,
    "unused lifetimes in function definitions"
}

#[derive(Copy, Clone)]
pub struct LifetimePass;

impl LintPass for LifetimePass {
    fn get_lints(&self) -> LintArray {
        lint_array!(NEEDLESS_LIFETIMES, EXTRA_UNUSED_LIFETIMES)
    }
}

impl<'a, 'tcx> LateLintPass<'a, 'tcx> for LifetimePass {
    fn check_item(&mut self, cx: &LateContext<'a, 'tcx>, item: &'tcx Item) {
        if let ItemFn(ref decl, _, ref generics, id) = item.node {
            check_fn_inner(cx, decl, Some(id), generics, item.span);
        }
    }

    fn check_impl_item(&mut self, cx: &LateContext<'a, 'tcx>, item: &'tcx ImplItem) {
        if let ImplItemKind::Method(ref sig, id) = item.node {
            check_fn_inner(cx, &sig.decl, Some(id), &item.generics, item.span);
        }
    }

    fn check_trait_item(&mut self, cx: &LateContext<'a, 'tcx>, item: &'tcx TraitItem) {
        if let TraitItemKind::Method(ref sig, ref body) = item.node {
            let body = match *body {
                TraitMethod::Required(_) => None,
                TraitMethod::Provided(id) => Some(id),
            };
            check_fn_inner(cx, &sig.decl, body, &item.generics, item.span);
        }
    }
}

/// The lifetime of a &-reference.
#[derive(PartialEq, Eq, Hash, Debug)]
enum RefLt {
    Unnamed,
    Static,
    Named(Name),
}

fn check_fn_inner<'a, 'tcx>(
    cx: &LateContext<'a, 'tcx>,
    decl: &'tcx FnDecl,
    body: Option<BodyId>,
    generics: &'tcx Generics,
    span: Span,
) {
    if in_external_macro(cx, span) || has_where_lifetimes(cx, &generics.where_clause) {
        return;
    }

    let mut bounds_lts = Vec::new();
    let types = generics.params.iter().filter_map(|param| match param.kind {
        GenericParamKind::Type { .. } => Some(param),
        GenericParamKind::Lifetime { .. } => None,
    });
    for typ in types {
        for bound in &typ.bounds {
            let mut visitor = RefVisitor::new(cx);
            walk_param_bound(&mut visitor, bound);
            if visitor.lts.iter().any(|lt| matches!(lt, RefLt::Named(_))) {
                return;
            }
            if let GenericBound::Trait(ref trait_ref, _) = *bound {
                let params = &trait_ref
                    .trait_ref
                    .path
                    .segments
                    .last()
                    .expect("a path must have at least one segment")
                    .args;
                if let Some(ref params) = *params {
                    let lifetimes = params.args.iter().filter_map(|arg| match arg {
                        GenericArg::Lifetime(lt) => Some(lt),
                        GenericArg::Type(_) => None,
                    });
                    for bound in lifetimes {
                        if bound.name != LifetimeName::Static && !bound.is_elided() {
                            return;
                        }
                        bounds_lts.push(bound);
                    }
                }
            }
        }
    }
    if could_use_elision(cx, decl, body, &generics.params, bounds_lts) {
        span_lint(
            cx,
            NEEDLESS_LIFETIMES,
            span,
            "explicit lifetimes given in parameter types where they could be elided",
        );
    }
    report_extra_lifetimes(cx, decl, generics);
}

fn could_use_elision<'a, 'tcx: 'a>(
    cx: &LateContext<'a, 'tcx>,
    func: &'tcx FnDecl,
    body: Option<BodyId>,
    named_generics: &'tcx [GenericParam],
    bounds_lts: Vec<&'tcx Lifetime>,
) -> bool {
    // There are two scenarios where elision works:
    // * no output references, all input references have different LT
    // * output references, exactly one input reference with same LT
    // All lifetimes must be unnamed, 'static or defined without bounds on the
    // level of the current item.

    // check named LTs
    let allowed_lts = allowed_lts_from(named_generics);

    // these will collect all the lifetimes for references in arg/return types
    let mut input_visitor = RefVisitor::new(cx);
    let mut output_visitor = RefVisitor::new(cx);

    // extract lifetimes in input argument types
    for arg in &func.inputs {
        input_visitor.visit_ty(arg);
    }
    // extract lifetimes in output type
    if let Return(ref ty) = func.output {
        output_visitor.visit_ty(ty);
    }

    let input_lts = match input_visitor.into_vec() {
        Some(lts) => lts_from_bounds(lts, bounds_lts.into_iter()),
        None => return false,
    };
    let output_lts = match output_visitor.into_vec() {
        Some(val) => val,
        None => return false,
    };

    if let Some(body_id) = body {
        let mut checker = BodyLifetimeChecker {
            lifetimes_used_in_body: false,
        };
        checker.visit_expr(&cx.tcx.hir.body(body_id).value);
        if checker.lifetimes_used_in_body {
            return false;
        }
    }

    // check for lifetimes from higher scopes
    for lt in input_lts.iter().chain(output_lts.iter()) {
        if !allowed_lts.contains(lt) {
            return false;
        }
    }

    // no input lifetimes? easy case!
    if input_lts.is_empty() {
        false
    } else if output_lts.is_empty() {
        // no output lifetimes, check distinctness of input lifetimes

        // only unnamed and static, ok
        let unnamed_and_static = input_lts
            .iter()
            .all(|lt| *lt == RefLt::Unnamed || *lt == RefLt::Static);
        if unnamed_and_static {
            return false;
        }
        // we have no output reference, so we only need all distinct lifetimes
        input_lts.len() == unique_lifetimes(&input_lts)
    } else {
        // we have output references, so we need one input reference,
        // and all output lifetimes must be the same
        if unique_lifetimes(&output_lts) > 1 {
            return false;
        }
        if input_lts.len() == 1 {
            match (&input_lts[0], &output_lts[0]) {
                (&RefLt::Named(n1), &RefLt::Named(n2)) if n1 == n2 => true,
                (&RefLt::Named(_), &RefLt::Unnamed) => true,
                _ => false, /* already elided, different named lifetimes
                             * or something static going on */
            }
        } else {
            false
        }
    }
}

fn allowed_lts_from(named_generics: &[GenericParam]) -> HashSet<RefLt> {
    let mut allowed_lts = HashSet::new();
    for par in named_generics.iter() {
        if let GenericParamKind::Lifetime { .. } = par.kind {
            if par.bounds.is_empty() {
                allowed_lts.insert(RefLt::Named(par.name.ident().name));
            }
        }
    }
    allowed_lts.insert(RefLt::Unnamed);
    allowed_lts.insert(RefLt::Static);
    allowed_lts
}

fn lts_from_bounds<'a, T: Iterator<Item = &'a Lifetime>>(mut vec: Vec<RefLt>, bounds_lts: T) -> Vec<RefLt> {
    for lt in bounds_lts {
        if lt.name != LifetimeName::Static {
            vec.push(RefLt::Named(lt.name.ident().name));
        }
    }

    vec
}

/// Number of unique lifetimes in the given vector.
fn unique_lifetimes(lts: &[RefLt]) -> usize {
    lts.iter().collect::<HashSet<_>>().len()
}

/// A visitor usable for `rustc_front::visit::walk_ty()`.
struct RefVisitor<'a, 'tcx: 'a> {
    cx: &'a LateContext<'a, 'tcx>,
    lts: Vec<RefLt>,
    abort: bool,
}

impl<'v, 't> RefVisitor<'v, 't> {
    fn new(cx: &'v LateContext<'v, 't>) -> Self {
        Self {
            cx,
            lts: Vec::new(),
            abort: false,
        }
    }

    fn record(&mut self, lifetime: &Option<Lifetime>) {
        if let Some(ref lt) = *lifetime {
            if lt.name == LifetimeName::Static {
                self.lts.push(RefLt::Static);
            } else if lt.is_elided() {
                self.lts.push(RefLt::Unnamed);
            } else {
                self.lts.push(RefLt::Named(lt.name.ident().name));
            }
        } else {
            self.lts.push(RefLt::Unnamed);
        }
    }

    fn into_vec(self) -> Option<Vec<RefLt>> {
        if self.abort {
            None
        } else {
            Some(self.lts)
        }
    }

    fn collect_anonymous_lifetimes(&mut self, qpath: &QPath, ty: &Ty) {
        if let Some(ref last_path_segment) = last_path_segment(qpath).args {
            if !last_path_segment.parenthesized
                && !last_path_segment.args.iter().any(|arg| match arg {
                    GenericArg::Lifetime(_) => true,
                    GenericArg::Type(_) => false,
                }) {
                let hir_id = self.cx.tcx.hir.node_to_hir_id(ty.id);
                match self.cx.tables.qpath_def(qpath, hir_id) {
                    Def::TyAlias(def_id) | Def::Struct(def_id) => {
                        let generics = self.cx.tcx.generics_of(def_id);
                        for _ in generics.params.as_slice() {
                            self.record(&None);
                        }
                    },
                    Def::Trait(def_id) => {
                        let trait_def = self.cx.tcx.trait_def(def_id);
                        for _ in &self.cx.tcx.generics_of(trait_def.def_id).params {
                            self.record(&None);
                        }
                    },
                    _ => (),
                }
            }
        }
    }
}

impl<'a, 'tcx> Visitor<'tcx> for RefVisitor<'a, 'tcx> {
    // for lifetimes as parameters of generics
    fn visit_lifetime(&mut self, lifetime: &'tcx Lifetime) {
        self.record(&Some(*lifetime));
    }

    fn visit_ty(&mut self, ty: &'tcx Ty) {
        match ty.node {
            TyRptr(ref lt, _) if lt.is_elided() => {
                self.record(&None);
            },
            TyPath(ref path) => {
                if let QPath::Resolved(_, ref path) = *path {
                    if let Def::Existential(def_id) = path.def {
                        let node_id = self.cx.tcx.hir.as_local_node_id(def_id).unwrap();
                        if let ItemExistential(ref exist_ty) = self.cx.tcx.hir.expect_item(node_id).node {
                            for bound in &exist_ty.bounds {
                                if let GenericBound::Outlives(_) = *bound {
                                    self.record(&None);
                                }
                            }
                        } else {
                            unreachable!()
                        }
                        walk_ty(self, ty);
                        return;
                    }
                }
                self.collect_anonymous_lifetimes(path, ty);
            }
            TyTraitObject(ref bounds, ref lt) => {
                if !lt.is_elided() {
                    self.abort = true;
                }
                for bound in bounds {
                    self.visit_poly_trait_ref(bound, TraitBoundModifier::None);
                }
                return;
            },
            _ => (),
        }
        walk_ty(self, ty);
    }
    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'tcx> {
        NestedVisitorMap::None
    }
}

/// Are any lifetimes mentioned in the `where` clause? If yes, we don't try to
/// reason about elision.
fn has_where_lifetimes<'a, 'tcx: 'a>(cx: &LateContext<'a, 'tcx>, where_clause: &'tcx WhereClause) -> bool {
    for predicate in &where_clause.predicates {
        match *predicate {
            WherePredicate::RegionPredicate(..) => return true,
            WherePredicate::BoundPredicate(ref pred) => {
                // a predicate like F: Trait or F: for<'a> Trait<'a>
                let mut visitor = RefVisitor::new(cx);
                // walk the type F, it may not contain LT refs
                walk_ty(&mut visitor, &pred.bounded_ty);
                if !visitor.lts.is_empty() {
                    return true;
                }
                // if the bounds define new lifetimes, they are fine to occur
                let allowed_lts = allowed_lts_from(&pred.bound_generic_params);
                // now walk the bounds
                for bound in pred.bounds.iter() {
                    walk_param_bound(&mut visitor, bound);
                }
                // and check that all lifetimes are allowed
                match visitor.into_vec() {
                    None => return false,
                    Some(lts) => for lt in lts {
                        if !allowed_lts.contains(&lt) {
                            return true;
                        }
                    },
                }
            },
            WherePredicate::EqPredicate(ref pred) => {
                let mut visitor = RefVisitor::new(cx);
                walk_ty(&mut visitor, &pred.lhs_ty);
                walk_ty(&mut visitor, &pred.rhs_ty);
                if !visitor.lts.is_empty() {
                    return true;
                }
            },
        }
    }
    false
}

struct LifetimeChecker {
    map: HashMap<Name, Span>,
}

impl<'tcx> Visitor<'tcx> for LifetimeChecker {
    // for lifetimes as parameters of generics
    fn visit_lifetime(&mut self, lifetime: &'tcx Lifetime) {
        self.map.remove(&lifetime.name.ident().name);
    }

    fn visit_generic_param(&mut self, param: &'tcx GenericParam) {
        // don't actually visit `<'a>` or `<'a: 'b>`
        // we've already visited the `'a` declarations and
        // don't want to spuriously remove them
        // `'b` in `'a: 'b` is useless unless used elsewhere in
        // a non-lifetime bound
        if let GenericParamKind::Type { .. } = param.kind {
            walk_generic_param(self, param)
        }
    }
    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'tcx> {
        NestedVisitorMap::None
    }
}

fn report_extra_lifetimes<'a, 'tcx: 'a>(cx: &LateContext<'a, 'tcx>, func: &'tcx FnDecl, generics: &'tcx Generics) {
    let hs = generics.params.iter()
        .filter_map(|par| match par.kind {
            GenericParamKind::Lifetime { .. } => Some((par.name.ident().name, par.span)),
            _ => None,
        })
        .collect();
    let mut checker = LifetimeChecker { map: hs };

    walk_generics(&mut checker, generics);
    walk_fn_decl(&mut checker, func);

    for &v in checker.map.values() {
        span_lint(cx, EXTRA_UNUSED_LIFETIMES, v, "this lifetime isn't used in the function definition");
    }
}

struct BodyLifetimeChecker {
    lifetimes_used_in_body: bool,
}

impl<'tcx> Visitor<'tcx> for BodyLifetimeChecker {
    // for lifetimes as parameters of generics
    fn visit_lifetime(&mut self, lifetime: &'tcx Lifetime) {
        if lifetime.name.ident().name != keywords::Invalid.name() && lifetime.name.ident().name != "'static" {
            self.lifetimes_used_in_body = true;
        }
    }

    fn nested_visit_map<'this>(&'this mut self) -> NestedVisitorMap<'this, 'tcx> {
        NestedVisitorMap::None
    }
}

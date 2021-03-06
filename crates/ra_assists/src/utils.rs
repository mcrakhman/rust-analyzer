//! Assorted functions shared by several assists.
pub(crate) mod insert_use;

use std::iter;

use hir::{Adt, Crate, Semantics, Trait, Type};
use ra_ide_db::RootDatabase;
use ra_syntax::{
    ast::{self, make, NameOwner},
    AstNode, T,
};
use rustc_hash::FxHashSet;

pub(crate) use insert_use::insert_use_statement;

pub fn get_missing_assoc_items(
    sema: &Semantics<RootDatabase>,
    impl_def: &ast::ImplDef,
) -> Vec<hir::AssocItem> {
    // Names must be unique between constants and functions. However, type aliases
    // may share the same name as a function or constant.
    let mut impl_fns_consts = FxHashSet::default();
    let mut impl_type = FxHashSet::default();

    if let Some(item_list) = impl_def.item_list() {
        for item in item_list.assoc_items() {
            match item {
                ast::AssocItem::FnDef(f) => {
                    if let Some(n) = f.name() {
                        impl_fns_consts.insert(n.syntax().to_string());
                    }
                }

                ast::AssocItem::TypeAliasDef(t) => {
                    if let Some(n) = t.name() {
                        impl_type.insert(n.syntax().to_string());
                    }
                }

                ast::AssocItem::ConstDef(c) => {
                    if let Some(n) = c.name() {
                        impl_fns_consts.insert(n.syntax().to_string());
                    }
                }
            }
        }
    }

    resolve_target_trait(sema, impl_def).map_or(vec![], |target_trait| {
        target_trait
            .items(sema.db)
            .iter()
            .filter(|i| match i {
                hir::AssocItem::Function(f) => {
                    !impl_fns_consts.contains(&f.name(sema.db).to_string())
                }
                hir::AssocItem::TypeAlias(t) => !impl_type.contains(&t.name(sema.db).to_string()),
                hir::AssocItem::Const(c) => c
                    .name(sema.db)
                    .map(|n| !impl_fns_consts.contains(&n.to_string()))
                    .unwrap_or_default(),
            })
            .cloned()
            .collect()
    })
}

pub(crate) fn resolve_target_trait(
    sema: &Semantics<RootDatabase>,
    impl_def: &ast::ImplDef,
) -> Option<hir::Trait> {
    let ast_path = impl_def
        .target_trait()
        .map(|it| it.syntax().clone())
        .and_then(ast::PathType::cast)?
        .path()?;

    match sema.resolve_path(&ast_path) {
        Some(hir::PathResolution::Def(hir::ModuleDef::Trait(def))) => Some(def),
        _ => None,
    }
}

pub(crate) fn invert_boolean_expression(expr: ast::Expr) -> ast::Expr {
    if let Some(expr) = invert_special_case(&expr) {
        return expr;
    }
    make::expr_prefix(T![!], expr)
}

fn invert_special_case(expr: &ast::Expr) -> Option<ast::Expr> {
    match expr {
        ast::Expr::BinExpr(bin) => match bin.op_kind()? {
            ast::BinOp::NegatedEqualityTest => bin.replace_op(T![==]).map(|it| it.into()),
            ast::BinOp::EqualityTest => bin.replace_op(T![!=]).map(|it| it.into()),
            _ => None,
        },
        ast::Expr::PrefixExpr(pe) if pe.op_kind()? == ast::PrefixOp::Not => pe.expr(),
        // FIXME:
        // ast::Expr::Literal(true | false )
        _ => None,
    }
}

#[derive(Clone, Copy)]
pub enum TryEnum {
    Result,
    Option,
}

impl TryEnum {
    const ALL: [TryEnum; 2] = [TryEnum::Option, TryEnum::Result];

    pub fn from_ty(sema: &Semantics<RootDatabase>, ty: &Type) -> Option<TryEnum> {
        let enum_ = match ty.as_adt() {
            Some(Adt::Enum(it)) => it,
            _ => return None,
        };
        TryEnum::ALL.iter().find_map(|&var| {
            if &enum_.name(sema.db).to_string() == var.type_name() {
                return Some(var);
            }
            None
        })
    }

    pub(crate) fn happy_case(self) -> &'static str {
        match self {
            TryEnum::Result => "Ok",
            TryEnum::Option => "Some",
        }
    }

    pub(crate) fn sad_pattern(self) -> ast::Pat {
        match self {
            TryEnum::Result => make::tuple_struct_pat(
                make::path_unqualified(make::path_segment(make::name_ref("Err"))),
                iter::once(make::placeholder_pat().into()),
            )
            .into(),
            TryEnum::Option => make::bind_pat(make::name("None")).into(),
        }
    }

    fn type_name(self) -> &'static str {
        match self {
            TryEnum::Result => "Result",
            TryEnum::Option => "Option",
        }
    }
}

/// Helps with finding well-know things inside the standard library. This is
/// somewhat similar to the known paths infra inside hir, but it different; We
/// want to make sure that IDE specific paths don't become interesting inside
/// the compiler itself as well.
pub(crate) struct FamousDefs<'a, 'b>(pub(crate) &'a Semantics<'b, RootDatabase>, pub(crate) Crate);

#[allow(non_snake_case)]
impl FamousDefs<'_, '_> {
    #[cfg(test)]
    pub(crate) const FIXTURE: &'static str = r#"
//- /libcore.rs crate:core
pub mod convert{
    pub trait From<T> {
        fn from(T) -> Self;
    }
}

pub mod prelude { pub use crate::convert::From }
#[prelude_import]
pub use prelude::*;
"#;

    pub(crate) fn core_convert_From(&self) -> Option<Trait> {
        self.find_trait("core:convert:From")
    }

    fn find_trait(&self, path: &str) -> Option<Trait> {
        let db = self.0.db;
        let mut path = path.split(':');
        let trait_ = path.next_back()?;
        let std_crate = path.next()?;
        let std_crate = self
            .1
            .dependencies(db)
            .into_iter()
            .find(|dep| &dep.name.to_string() == std_crate)?
            .krate;

        let mut module = std_crate.root_module(db)?;
        for segment in path {
            module = module.children(db).find_map(|child| {
                let name = child.name(db)?;
                if &name.to_string() == segment {
                    Some(child)
                } else {
                    None
                }
            })?;
        }
        let def =
            module.scope(db, None).into_iter().find(|(name, _def)| &name.to_string() == trait_)?.1;
        match def {
            hir::ScopeDef::ModuleDef(hir::ModuleDef::Trait(it)) => Some(it),
            _ => None,
        }
    }
}

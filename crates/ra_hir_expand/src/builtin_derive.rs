//! Builtin derives.

use log::debug;

use ra_parser::FragmentKind;
use ra_syntax::{
    ast::{self, AstNode, ModuleItemOwner, NameOwner, TypeParamsOwner},
    match_ast,
};

use crate::db::AstDatabase;
use crate::{name, quote, MacroCallId, MacroDefId, MacroDefKind};

macro_rules! register_builtin {
    ( $(($name:ident, $kind: ident) => $expand:ident),* ) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum BuiltinDeriveExpander {
            $($kind),*
        }

        impl BuiltinDeriveExpander {
            pub fn expand(
                &self,
                db: &dyn AstDatabase,
                id: MacroCallId,
                tt: &tt::Subtree,
            ) -> Result<tt::Subtree, mbe::ExpandError> {
                let expander = match *self {
                    $( BuiltinDeriveExpander::$kind => $expand, )*
                };
                expander(db, id, tt)
            }
        }

        pub fn find_builtin_derive(ident: &name::Name) -> Option<MacroDefId> {
            let kind = match ident {
                 $( id if id == &name::$name => BuiltinDeriveExpander::$kind, )*
                 _ => return None,
            };

            Some(MacroDefId { krate: None, ast_id: None, kind: MacroDefKind::BuiltInDerive(kind) })
        }
    };
}

register_builtin! {
    (COPY_TRAIT, Copy) => copy_expand,
    (CLONE_TRAIT, Clone) => clone_expand,
    (DEFAULT_TRAIT, Default) => default_expand,
    (DEBUG_TRAIT, Debug) => debug_expand,
    (HASH_TRAIT, Hash) => hash_expand,
    (ORD_TRAIT, Ord) => ord_expand,
    (PARTIAL_ORD_TRAIT, PartialOrd) => partial_ord_expand,
    (EQ_TRAIT, Eq) => eq_expand,
    (PARTIAL_EQ_TRAIT, PartialEq) => partial_eq_expand
}

struct BasicAdtInfo {
    name: tt::Ident,
    type_params: usize,
}

fn parse_adt(tt: &tt::Subtree) -> Result<BasicAdtInfo, mbe::ExpandError> {
    let (parsed, token_map) = mbe::token_tree_to_syntax_node(tt, FragmentKind::Items)?; // FragmentKind::Items doesn't parse attrs?
    let macro_items = ast::MacroItems::cast(parsed.syntax_node()).ok_or_else(|| {
        debug!("derive node didn't parse");
        mbe::ExpandError::UnexpectedToken
    })?;
    let item = macro_items.items().next().ok_or_else(|| {
        debug!("no module item parsed");
        mbe::ExpandError::NoMatchingRule
    })?;
    let node = item.syntax();
    let (name, params) = match_ast! {
        match node {
            ast::StructDef(it) => { (it.name(), it.type_param_list()) },
            ast::EnumDef(it) => { (it.name(), it.type_param_list()) },
            ast::UnionDef(it) => { (it.name(), it.type_param_list()) },
            _ => {
                debug!("unexpected node is {:?}", node);
                return Err(mbe::ExpandError::ConversionError)
            },
        }
    };
    let name = name.ok_or_else(|| {
        debug!("parsed item has no name");
        mbe::ExpandError::NoMatchingRule
    })?;
    let name_token_id = token_map.token_by_range(name.syntax().text_range()).ok_or_else(|| {
        debug!("name token not found");
        mbe::ExpandError::ConversionError
    })?;
    let name_token = tt::Ident { id: name_token_id, text: name.text().clone() };
    let type_params = params.map_or(0, |type_param_list| type_param_list.type_params().count());
    Ok(BasicAdtInfo { name: name_token, type_params })
}

fn make_type_args(n: usize, bound: Vec<tt::TokenTree>) -> Vec<tt::TokenTree> {
    let mut result = Vec::<tt::TokenTree>::new();
    result.push(tt::Leaf::Punct(tt::Punct { char: '<', spacing: tt::Spacing::Alone }).into());
    for i in 0..n {
        if i > 0 {
            result
                .push(tt::Leaf::Punct(tt::Punct { char: ',', spacing: tt::Spacing::Alone }).into());
        }
        result.push(
            tt::Leaf::Ident(tt::Ident {
                id: tt::TokenId::unspecified(),
                text: format!("T{}", i).into(),
            })
            .into(),
        );
        result.extend(bound.iter().cloned());
    }
    result.push(tt::Leaf::Punct(tt::Punct { char: '>', spacing: tt::Spacing::Alone }).into());
    result
}

fn expand_simple_derive(
    tt: &tt::Subtree,
    trait_path: tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    let info = parse_adt(tt)?;
    let name = info.name;
    let trait_path_clone = trait_path.token_trees.clone();
    let bound = (quote! { : ##trait_path_clone }).token_trees;
    let type_params = make_type_args(info.type_params, bound);
    let type_args = make_type_args(info.type_params, Vec::new());
    let trait_path = trait_path.token_trees;
    let expanded = quote! {
        impl ##type_params ##trait_path for #name ##type_args {}
    };
    Ok(expanded)
}

fn copy_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::marker::Copy })
}

fn clone_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::clone::Clone })
}

fn default_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::default::Default })
}

fn debug_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::fmt::Debug })
}

fn hash_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::hash::Hash })
}

fn eq_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::cmp::Eq })
}

fn partial_eq_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::cmp::PartialEq })
}

fn ord_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::cmp::Ord })
}

fn partial_ord_expand(
    _db: &dyn AstDatabase,
    _id: MacroCallId,
    tt: &tt::Subtree,
) -> Result<tt::Subtree, mbe::ExpandError> {
    expand_simple_derive(tt, quote! { std::cmp::PartialOrd })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{test_db::TestDB, AstId, MacroCallKind, MacroCallLoc};
    use ra_db::{fixture::WithFixture, SourceDatabase};

    fn expand_builtin_derive(s: &str, expander: BuiltinDeriveExpander) -> String {
        let (db, file_id) = TestDB::with_single_file(&s);
        let parsed = db.parse(file_id);
        let items: Vec<_> =
            parsed.syntax_node().descendants().filter_map(|it| ast::ModuleItem::cast(it)).collect();

        let ast_id_map = db.ast_id_map(file_id.into());

        // the first one should be a macro_rules
        let def =
            MacroDefId { krate: None, ast_id: None, kind: MacroDefKind::BuiltInDerive(expander) };

        let loc = MacroCallLoc {
            def,
            kind: MacroCallKind::Attr(AstId::new(file_id.into(), ast_id_map.ast_id(&items[0]))),
        };

        let id = db.intern_macro(loc);
        let parsed = db.parse_or_expand(id.as_file()).unwrap();

        // FIXME text() for syntax nodes parsed from token tree looks weird
        // because there's no whitespace, see below
        parsed.text().to_string()
    }

    #[test]
    fn test_copy_expand_simple() {
        let expanded = expand_builtin_derive(
            r#"
        #[derive(Copy)]
        struct Foo;
"#,
            BuiltinDeriveExpander::Copy,
        );

        assert_eq!(expanded, "impl <>std::marker::CopyforFoo <>{}");
    }

    #[test]
    fn test_copy_expand_with_type_params() {
        let expanded = expand_builtin_derive(
            r#"
        #[derive(Copy)]
        struct Foo<A, B>;
"#,
            BuiltinDeriveExpander::Copy,
        );

        assert_eq!(
            expanded,
            "impl<T0:std::marker::Copy,T1:std::marker::Copy>std::marker::CopyforFoo<T0,T1>{}"
        );
    }

    #[test]
    fn test_copy_expand_with_lifetimes() {
        let expanded = expand_builtin_derive(
            r#"
        #[derive(Copy)]
        struct Foo<A, B, 'a, 'b>;
"#,
            BuiltinDeriveExpander::Copy,
        );

        // We currently just ignore lifetimes

        assert_eq!(
            expanded,
            "impl<T0:std::marker::Copy,T1:std::marker::Copy>std::marker::CopyforFoo<T0,T1>{}"
        );
    }

    #[test]
    fn test_clone_expand() {
        let expanded = expand_builtin_derive(
            r#"
        #[derive(Clone)]
        struct Foo<A, B>;
"#,
            BuiltinDeriveExpander::Clone,
        );

        assert_eq!(
            expanded,
            "impl<T0:std::clone::Clone,T1:std::clone::Clone>std::clone::CloneforFoo<T0,T1>{}"
        );
    }
}

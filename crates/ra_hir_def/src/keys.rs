//! keys to be used with `DynMap`

use std::marker::PhantomData;

use hir_expand::InFile;
use ra_syntax::{ast, AstNode, AstPtr};
use rustc_hash::FxHashMap;

use crate::{
    dyn_map::{DynMap, Policy},
    ConstId, EnumVariantId, FunctionId, StaticId, StructFieldId, TypeAliasId, TypeParamId,
};

type Key<K, V> = crate::dyn_map::Key<InFile<K>, V, AstPtrPolicy<K, V>>;

pub const FUNCTION: Key<ast::FnDef, FunctionId> = Key::new();
pub const CONST: Key<ast::ConstDef, ConstId> = Key::new();
pub const STATIC: Key<ast::StaticDef, StaticId> = Key::new();
pub const ENUM_VARIANT: Key<ast::EnumVariant, EnumVariantId> = Key::new();
pub const TYPE_ALIAS: Key<ast::TypeAliasDef, TypeAliasId> = Key::new();
pub const TUPLE_FIELD: Key<ast::TupleFieldDef, StructFieldId> = Key::new();
pub const RECORD_FIELD: Key<ast::RecordFieldDef, StructFieldId> = Key::new();
pub const TYPE_PARAM: Key<ast::TypeParam, TypeParamId> = Key::new();

/// XXX: AST Nodes and SyntaxNodes have identity equality semantics: nodes are
/// equal if they point to exactly the same object.
///
/// In general, we do not guarantee that we have exactly one instance of a
/// syntax tree for each file. We probably should add such guarantee, but, for
/// the time being, we will use identity-less AstPtr comparison.
pub struct AstPtrPolicy<AST, ID> {
    _phantom: PhantomData<(AST, ID)>,
}

impl<AST: AstNode + 'static, ID: 'static> Policy for AstPtrPolicy<AST, ID> {
    type K = InFile<AST>;
    type V = ID;
    fn insert(map: &mut DynMap, key: InFile<AST>, value: ID) {
        let key = key.as_ref().map(AstPtr::new);
        map.map
            .entry::<FxHashMap<InFile<AstPtr<AST>>, ID>>()
            .or_insert_with(Default::default)
            .insert(key, value);
    }
    fn get<'a>(map: &'a DynMap, key: &InFile<AST>) -> Option<&'a ID> {
        let key = key.as_ref().map(AstPtr::new);
        map.map.get::<FxHashMap<InFile<AstPtr<AST>>, ID>>()?.get(&key)
    }
}

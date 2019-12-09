//! This modules implements "expand macro" functionality in the IDE

use crate::{db::RootDatabase, FilePosition};
use hir::db::AstDatabase;
use ra_db::SourceDatabase;
use rustc_hash::FxHashMap;

use ra_syntax::{
    algo::{find_node_at_offset, replace_descendants},
    ast::{self},
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, WalkEvent, T,
};

pub struct ExpandedMacro {
    pub name: String,
    pub expansion: String,
}

pub(crate) fn expand_macro(db: &RootDatabase, position: FilePosition) -> Option<ExpandedMacro> {
    let parse = db.parse(position.file_id);
    let file = parse.tree();
    let name_ref = find_node_at_offset::<ast::NameRef>(file.syntax(), position.offset)?;
    let mac = name_ref.syntax().ancestors().find_map(ast::MacroCall::cast)?;

    let source = hir::InFile::new(position.file_id.into(), mac.syntax());
    let expanded = expand_macro_recur(db, source, source.with_value(&mac))?;

    // FIXME:
    // macro expansion may lose all white space information
    // But we hope someday we can use ra_fmt for that
    let expansion = insert_whitespaces(expanded);
    Some(ExpandedMacro { name: name_ref.text().to_string(), expansion })
}

fn expand_macro_recur(
    db: &RootDatabase,
    source: hir::InFile<&SyntaxNode>,
    macro_call: hir::InFile<&ast::MacroCall>,
) -> Option<SyntaxNode> {
    let analyzer = hir::SourceAnalyzer::new(db, source, None);
    let expansion = analyzer.expand(db, macro_call)?;
    let macro_file_id = expansion.file_id();
    let mut expanded: SyntaxNode = db.parse_or_expand(macro_file_id)?;

    let children = expanded.descendants().filter_map(ast::MacroCall::cast);
    let mut replaces = FxHashMap::default();

    for child in children.into_iter() {
        let node = hir::InFile::new(macro_file_id, &child);
        if let Some(new_node) = expand_macro_recur(db, source, node) {
            // Replace the whole node if it is root
            // `replace_descendants` will not replace the parent node
            // but `SyntaxNode::descendants include itself
            if expanded == *child.syntax() {
                expanded = new_node;
            } else {
                replaces.insert(child.syntax().clone().into(), new_node.into());
            }
        }
    }

    Some(replace_descendants(&expanded, &replaces))
}

// FIXME: It would also be cool to share logic here and in the mbe tests,
// which are pretty unreadable at the moment.
fn insert_whitespaces(syn: SyntaxNode) -> String {
    use SyntaxKind::*;

    let mut res = String::new();
    let mut token_iter = syn
        .preorder_with_tokens()
        .filter_map(|event| {
            if let WalkEvent::Enter(NodeOrToken::Token(token)) = event {
                Some(token)
            } else {
                None
            }
        })
        .peekable();

    let mut indent = 0;
    let mut last: Option<SyntaxKind> = None;

    while let Some(token) = token_iter.next() {
        let mut is_next = |f: fn(SyntaxKind) -> bool, default| -> bool {
            token_iter.peek().map(|it| f(it.kind())).unwrap_or(default)
        };
        let is_last = |f: fn(SyntaxKind) -> bool, default| -> bool {
            last.map(|it| f(it)).unwrap_or(default)
        };

        res += &match token.kind() {
            k @ _ if is_text(k) && is_next(|it| !it.is_punct(), true) => {
                token.text().to_string() + " "
            }
            L_CURLY if is_next(|it| it != R_CURLY, true) => {
                indent += 1;
                let leading_space = if is_last(|it| is_text(it), false) { " " } else { "" };
                format!("{}{{\n{}", leading_space, "  ".repeat(indent))
            }
            R_CURLY if is_last(|it| it != L_CURLY, true) => {
                indent = indent.checked_sub(1).unwrap_or(0);
                format!("\n{}}}", "  ".repeat(indent))
            }
            R_CURLY => format!("}}\n{}", "  ".repeat(indent)),
            T![;] => format!(";\n{}", "  ".repeat(indent)),
            T![->] => " -> ".to_string(),
            T![=] => " = ".to_string(),
            T![=>] => " => ".to_string(),
            _ => token.text().to_string(),
        };

        last = Some(token.kind());
    }

    return res;

    fn is_text(k: SyntaxKind) -> bool {
        k.is_keyword() || k.is_literal() || k == IDENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock_analysis::analysis_and_position;
    use insta::assert_snapshot;

    fn check_expand_macro(fixture: &str) -> ExpandedMacro {
        let (analysis, pos) = analysis_and_position(fixture);
        analysis.expand_macro(pos).unwrap().unwrap()
    }

    #[test]
    fn macro_expand_recursive_expansion() {
        let res = check_expand_macro(
            r#"
        //- /lib.rs
        macro_rules! bar {
            () => { fn  b() {} }
        }
        macro_rules! foo {
            () => { bar!(); }
        }
        macro_rules! baz {
            () => { foo!(); }
        }
        f<|>oo!();
        "#,
        );

        assert_eq!(res.name, "foo");
        assert_snapshot!(res.expansion, @r###"
fn b(){}
"###);
    }

    #[test]
    fn macro_expand_multiple_lines() {
        let res = check_expand_macro(
            r#"
        //- /lib.rs
        macro_rules! foo {
            () => {
                fn some_thing() -> u32 {
                    let a = 0;
                    a + 10
                }
            }
        }
        f<|>oo!();
        "#,
        );

        assert_eq!(res.name, "foo");
        assert_snapshot!(res.expansion, @r###"
fn some_thing() -> u32 {
  let a = 0;
  a+10
}
"###);
    }

    #[test]
    fn macro_expand_match_ast() {
        let res = check_expand_macro(
            r#"
        //- /lib.rs
        macro_rules! match_ast {
            (match $node:ident { $($tt:tt)* }) => { match_ast!(match ($node) { $($tt)* }) };
        
            (match ($node:expr) {
                $( ast::$ast:ident($it:ident) => $res:block, )*
                _ => $catch_all:expr $(,)?
            }) => {{
                $( if let Some($it) = ast::$ast::cast($node.clone()) $res else )*
                { $catch_all }
            }};
        }        

        fn main() {
            mat<|>ch_ast! {
                match container {
                    ast::TraitDef(it) => {},
                    ast::ImplBlock(it) => {},
                    _ => { continue },
                }
            }
        }
        "#,
        );

        assert_eq!(res.name, "match_ast");
        assert_snapshot!(res.expansion, @r###"
{
  if let Some(it) = ast::TraitDef::cast(container.clone()){}
  else if let Some(it) = ast::ImplBlock::cast(container.clone()){}
  else {
    {
      continue
    }
  }
}
"###);
    }

    #[test]
    fn macro_expand_match_ast_inside_let_statement() {
        let res = check_expand_macro(
            r#"
        //- /lib.rs
        macro_rules! match_ast {
            (match $node:ident { $($tt:tt)* }) => { match_ast!(match ($node) { $($tt)* }) };        
            (match ($node:expr) {}) => {{}};
        }        

        fn main() {        
            let p = f(|it| {
                let res = mat<|>ch_ast! { match c {}};
                Some(res)
            })?;
        }
        "#,
        );

        assert_eq!(res.name, "match_ast");
        assert_snapshot!(res.expansion, @r###"{}"###);
    }

    #[test]
    fn macro_expand_inner_macro_fail_to_expand() {
        let res = check_expand_macro(
            r#"
        //- /lib.rs
        macro_rules! bar {
            (BAD) => {};
        }
        macro_rules! foo {
            () => {bar!()};
        }        

        fn main() {        
            let res = fo<|>o!();
        }
        "#,
        );

        assert_eq!(res.name, "foo");
        assert_snapshot!(res.expansion, @r###"bar!()"###);
    }

    #[test]
    fn macro_expand_with_dollar_crate() {
        let res = check_expand_macro(
            r#"
        //- /lib.rs
        #[macro_export]
        macro_rules! bar {
            () => {0};
        }
        macro_rules! foo {
            () => {$crate::bar!()};
        }        

        fn main() {        
            let res = fo<|>o!();
        }
        "#,
        );

        assert_eq!(res.name, "foo");
        assert_snapshot!(res.expansion, @r###"0"###);
    }
}

use std::collections::{HashSet, HashMap};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use swc_ecmascript::visit::{Fold, FoldWith, Visit, VisitWith, Node};
use swc_ecmascript::ast::*;
use swc_atoms::JsWord;
use swc_common::{DUMMY_SP, SyntaxContext, Mark, sync::Lrc};
use serde::{Deserialize, Serialize};

use crate::utils::{match_member_expr, SourceLocation};

type IdentId = (JsWord, SyntaxContext);
macro_rules! id {
  ($ident: expr) => {
    ($ident.sym.clone(), $ident.span.ctxt)
  };
}

macro_rules! hash {
  ($str:expr) => {
    {
      let mut hasher = DefaultHasher::new();
      $str.hash(&mut hasher);
      hasher.finish()
    }
  };
}

pub fn hoist(module: Module, source_map: Lrc<swc_common::SourceMap>, module_id: &str, decls: HashSet<IdentId>, ignore_mark: Mark, global_mark: Mark) -> (Module, HoistResult) {
  let mut collect = Collect::new(source_map, decls, ignore_mark);
  module.visit_with(&Invalid { span: DUMMY_SP } as _, &mut collect);

  let mut hoist = Hoist::new(module_id, &collect, global_mark);
  let module = module.fold_with(&mut hoist);
  (module, hoist.get_result())
}

struct Hoist<'a> {
  module_id: &'a str,
  collect: &'a Collect,
  global_ctxt: SyntaxContext,
  requires_in_stmt: Vec<ModuleItem>,
  export_decls: HashSet<JsWord>,
  imported_symbols: HashMap<JsWord, (JsWord, JsWord, SourceLocation)>,
  exported_symbols: HashMap<JsWord, (JsWord, SourceLocation)>,
  re_exports: Vec<(JsWord, JsWord, JsWord, SourceLocation)>,
  self_references: HashSet<JsWord>,
  dynamic_imports: HashMap<JsWord, JsWord>,
  in_function_scope: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HoistResult {
  imported_symbols: HashMap<JsWord, (JsWord, JsWord, SourceLocation)>,
  exported_symbols: HashMap<JsWord, (JsWord, SourceLocation)>,
  re_exports: Vec<(JsWord, JsWord, JsWord, SourceLocation)>,
  self_references: HashSet<JsWord>,
  wrapped_requires: HashSet<JsWord>,
  dynamic_imports: HashMap<JsWord, JsWord>,
  static_cjs_exports: bool,
  has_cjs_exports: bool,
  should_wrap: bool,
}

impl<'a> Hoist<'a> {
  fn new(module_id: &'a str, collect: &'a Collect, global_mark: Mark) -> Self {
    Hoist {
      module_id,
      collect,
      global_ctxt: SyntaxContext::empty().apply_mark(global_mark),
      requires_in_stmt: vec![],
      export_decls: HashSet::new(),
      imported_symbols: HashMap::new(),
      exported_symbols: HashMap::new(),
      re_exports: vec![],
      self_references: HashSet::new(),
      dynamic_imports: HashMap::new(),
      in_function_scope: false
    }
  }

  fn get_result(self) -> HoistResult {
    HoistResult {
      imported_symbols: self.imported_symbols,
      exported_symbols: self.exported_symbols,
      re_exports: self.re_exports,
      self_references: self.self_references,
      dynamic_imports: self.dynamic_imports,
      wrapped_requires: self.collect.wrapped_requires.clone(),
      static_cjs_exports: self.collect.static_cjs_exports,
      has_cjs_exports: self.collect.has_cjs_exports,
      should_wrap: self.collect.should_wrap,
    }
  }
}

impl<'a> Fold for Hoist<'a> {
  fn fold_module(&mut self, node: Module) -> Module {
    let mut node = node;
    let mut hoisted_imports = vec![];
    let mut items = vec![];
    for item in &node.body {
      match &item {
        ModuleItem::ModuleDecl(decl) => {
          match decl {
            ModuleDecl::Import(import) => {
              hoisted_imports.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                specifiers: vec![],
                asserts: None,
                span: DUMMY_SP,
                src: Str { value: format!("{}:{}", self.module_id, import.src.value).into(), span: DUMMY_SP, kind: StrKind::Synthesized, has_escape: false },
                type_only: false
              })));
            },
            ModuleDecl::ExportNamed(export) => {
              if let Some(src) = &export.src {
                // TODO: skip if already imported.
                hoisted_imports.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                  specifiers: vec![],
                  asserts: None,
                  span: DUMMY_SP,
                  src: Str { value: format!("{}:{}", self.module_id, src.value).into(), span: DUMMY_SP, kind: StrKind::Synthesized, has_escape: false },
                  type_only: false
                })));

                for specifier in &export.specifiers {
                  match specifier {
                    ExportSpecifier::Named(named) => {
                      let exported = match &named.exported {
                        Some(exported) => exported.sym.clone(),
                        None => named.orig.sym.clone()
                      };
                      self.re_exports.push((exported, src.value.clone(), named.orig.sym.clone(), SourceLocation::from(&self.collect.source_map, named.span)));
                    },
                    ExportSpecifier::Default(default) => {
                      self.re_exports.push((default.exported.sym.clone(), src.value.clone(), js_word!("default"), SourceLocation::from(&self.collect.source_map, default.exported.span)));
                    },
                    ExportSpecifier::Namespace(namespace) => {
                      self.re_exports.push((namespace.name.sym.clone(), src.value.clone(), "*".into(), SourceLocation::from(&self.collect.source_map, namespace.span)));
                    }
                  }
                }
              } else {
                for specifier in &export.specifiers {
                  match specifier {
                    ExportSpecifier::Named(named) => {
                      let exported = match &named.exported {
                        Some(exported) => exported.sym.clone(),
                        None => named.orig.sym.clone()
                      };
                      if let Some((source, local, _, _)) = self.collect.imports.get(&id!(named.orig)) {
                        self.re_exports.push((exported, source.clone(), local.clone(), SourceLocation::from(&self.collect.source_map, named.span)));
                      } else {
                        // A variable will appear only once in the `exports` mapping but
                        // could be exported multiple times with different names.
                        // Find the original exported name, and remap.
                        let orig_exported = self.collect.exports.get(&id!(named.orig)).unwrap();
                        let id = if self.collect.should_wrap {
                          Ident::new(orig_exported.clone(), DUMMY_SP)
                        } else {
                          self.get_export_ident(DUMMY_SP, orig_exported)
                        };
                        self.exported_symbols.entry(exported).or_insert((id.sym, SourceLocation::from(&self.collect.source_map, named.span)));
                      }
                    },
                    _ => {}
                  }
                }
              }
            },
            ModuleDecl::ExportAll(export) => {
              hoisted_imports.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                specifiers: vec![],
                asserts: None,
                span: DUMMY_SP,
                src: Str { value: format!("{}:{}", self.module_id, export.src.value).into(), span: DUMMY_SP, kind: StrKind::Synthesized, has_escape: false },
                type_only: false
              })));
              self.re_exports.push(("*".into(), export.src.value.clone(), "*".into(), SourceLocation::from(&self.collect.source_map, export.span)));
            },
            ModuleDecl::ExportDefaultExpr(export) => {
              let ident = self.get_export_ident(export.span, &"default".into());
              let init = export.expr.clone().fold_with(self);
              if self.requires_in_stmt.len() > 0 {
                items.append(&mut self.requires_in_stmt);
                self.requires_in_stmt.clear();
              }
              items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Var(VarDecl {
                declare: false,
                kind: VarDeclKind::Var,
                span: DUMMY_SP,
                decls: vec![
                  VarDeclarator {
                    definite: false,
                    span: DUMMY_SP,
                    name: Pat::Ident(BindingIdent::from(ident)),
                    init: Some(init)
                  }
                ]
              }))));
            },
            ModuleDecl::ExportDefaultDecl(export) => {
              let decl = match &export.decl {
                DefaultDecl::Class(class) => {
                  Decl::Class(ClassDecl {
                    ident: self.get_export_ident(DUMMY_SP, &"default".into()),
                    declare: false,
                    class: class.class.clone().fold_with(self)
                  })
                },
                DefaultDecl::Fn(func) => {
                  Decl::Fn(FnDecl {
                    ident: self.get_export_ident(DUMMY_SP, &"default".into()),
                    declare: false,
                    function: func.function.clone().fold_with(self)
                  })
                },
                _ => {
                  unreachable!("unsupported export default declaration");
                }
              };

              items.push(ModuleItem::Stmt(Stmt::Decl(decl)));
            },
            ModuleDecl::ExportDecl(export) => {
              items.push(ModuleItem::Stmt(Stmt::Decl(export.decl.clone().fold_with(self))));
            },
            _ => {
              items.push(item.clone().fold_with(self))
            }
          }
        },
        ModuleItem::Stmt(stmt) => {
          match stmt {
            Stmt::Decl(decl) => {
              match decl {
                Decl::Var(var) => {
                  let mut decls = vec![];
                  for v in &var.decls {
                    if let Some(init) = &v.init {
                      if let Some(source) = match_require(init, &self.collect.decls, self.collect.ignore_mark) {
                        // If the require is accessed in a way we cannot analyze, do not replace.
                        // e.g. const {x: {y: z}} = require('x');
                        // The require will be handled in the expression handler, below.
                        if !self.collect.non_static_requires.contains(&source) {
                          if decls.len() > 0 {
                            let mut var = var.clone();
                            var.decls = decls.clone();
                            items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Var(var))));
                            decls.clear();
                          }

                          items.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                            specifiers: vec![],
                            asserts: None,
                            span: DUMMY_SP,
                            src: Str { value: format!("{}:{}", self.module_id, source).into(), span: DUMMY_SP, kind: StrKind::Synthesized, has_escape: false },
                            type_only: false
                          })));
                          continue;
                        }
                      }

                      match &**init {
                        Expr::Member(member) => {
                          match &member.obj {
                            ExprOrSuper::Expr(expr) => {
                              if let Some(source) = match_require(&*expr, &self.collect.decls, self.collect.ignore_mark) {
                                if !self.collect.non_static_requires.contains(&source) {
                                  if decls.len() > 0 {
                                    let mut var = var.clone();
                                    var.decls = decls.clone();
                                    items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Var(var))));
                                    decls.clear();
                                  }
        
                                  items.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
                                    specifiers: vec![],
                                    asserts: None,
                                    span: DUMMY_SP,
                                    src: Str { value: format!("{}:{}", self.module_id, source).into(), span: DUMMY_SP, kind: StrKind::Synthesized, has_escape: false },
                                    type_only: false
                                  })));
                                  continue;
                                }
                              }
                            },
                            _ => {}
                          }
                        },
                        _ => {}
                      }
                    }
                    let d = v.clone().fold_with(self);
                    if self.requires_in_stmt.len() > 0 {
                      if decls.len() > 0 {
                        let mut var = var.clone();
                        var.decls = decls.clone();
                        items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Var(var))));
                        decls.clear();
                      }

                      items.append(&mut self.requires_in_stmt);
                      self.requires_in_stmt.clear();
                    }
                    decls.push(d);
                  }

                  if decls.len() > 0 {
                    let mut var = var.clone();
                    var.decls = decls;
                    items.push(ModuleItem::Stmt(Stmt::Decl(Decl::Var(var))))
                  }
                },
                _ => {
                  let d = item.clone().fold_with(self);
                  if self.requires_in_stmt.len() > 0 {
                    items.append(&mut self.requires_in_stmt);
                    self.requires_in_stmt.clear();
                  }
                  items.push(d)
                }
              }
            },
            _ => {
              let d = item.clone().fold_with(self);
              if self.requires_in_stmt.len() > 0 {
                items.append(&mut self.requires_in_stmt);
                self.requires_in_stmt.clear();
              }
              items.push(d)
            }
          }
        }
      }
    }

    for name in &self.export_decls {
      hoisted_imports.push(ModuleItem::Stmt(Stmt::Decl(Decl::Var(VarDecl {
        declare: false,
        kind: VarDeclKind::Var,
        span: node.span,
        decls: vec![
          VarDeclarator {
            definite: false,
            span: node.span,
            name: Pat::Ident(BindingIdent::from(Ident::new(name.clone(), DUMMY_SP))),
            init: None
          }
        ]
      }))));
    }

    items.splice(0..0, hoisted_imports);
    node.body = items;
    node
  }

  fn fold_function(&mut self, node: Function) -> Function {
    let in_function_scope = self.in_function_scope;
    self.in_function_scope = true;
    let res = node.fold_children_with(self);
    self.in_function_scope = in_function_scope;
    res
  }

  fn fold_class(&mut self, node: Class) -> Class {
    let in_function_scope = self.in_function_scope;
    self.in_function_scope = true;
    let res = node.fold_children_with(self);
    self.in_function_scope = in_function_scope;
    res
  }

  fn fold_expr(&mut self, node: Expr) -> Expr {
    match &node {
      Expr::Member(member) => {
        if !self.collect.should_wrap && match_member_expr(&member, vec!["module", "exports"], &self.collect.decls) {
          self.self_references.insert("*".into());
          return Expr::Ident(self.get_export_ident(member.span, &"*".into()))
        }

        let key = match &*member.prop {
          Expr::Ident(ident) => {
            if !member.computed {
              ident.sym.clone()
            } else {
              return node.fold_children_with(self);
            }
          },
          Expr::Lit(lit) => {
            match lit {
              Lit::Str(str_) => str_.value.clone(),
              _ => return node.fold_children_with(self)
            }
          },
          _ => return node.fold_children_with(self)
        };

        match &member.obj {
          ExprOrSuper::Expr(expr) => {
            match &**expr {
              Expr::Ident(ident) => {
                // import * as y from 'x'; OR const y = require('x'); OR const y = await import('x');
                // y.foo -> $id$import$10b1f2ceae7ab64e$y
                if let Some((source, local, is_async, _loc)) = self.collect.imports.get(&id!(ident)) {
                  // If there are any non-static accesses of the namespace, don't perform any replacement.
                  // This will be handled in the Ident visitor below, which replaces y -> $id$import$10b1f2ceae7ab64e.
                  if local == "*" && !self.collect.non_static_access.contains(&id!(ident)) && !self.collect.non_static_requires.contains(&source) {
                    if *is_async {
                      let name: JsWord = format!("${}$importAsync${:x}${:x}", self.module_id, hash!(source), hash!(key)).into();
                      self.imported_symbols.insert(name, (source.clone(), key.clone(), SourceLocation::from(&self.collect.source_map, member.span)));
                    } else {
                      return Expr::Ident(self.get_import_ident(member.span, &source, &key, SourceLocation::from(&self.collect.source_map, member.span)))
                    }
                  }
                }

                // exports.foo -> $id$export$foo
                let exports: JsWord = "exports".into();
                if ident.sym == exports && !self.collect.decls.contains(&id!(ident)) {
                  if self.collect.static_cjs_exports && !self.collect.should_wrap {
                    self.self_references.insert(key.clone());
                    return Expr::Ident(self.get_export_ident(member.span, &key))
                  }
                }
              },
              Expr::Call(call) => {
                // require('foo').bar -> $id$import$foo$bar
                if let Some(source) = match_require(expr, &self.collect.decls, self.collect.ignore_mark) {
                  self.add_require(&source);
                  return Expr::Ident(self.get_import_ident(member.span, &source, &key, SourceLocation::from(&self.collect.source_map, member.span)))
                }
              },
              Expr::Member(mem) => {
                // module.exports.foo -> $id$export$foo
                if self.collect.static_cjs_exports && !self.collect.should_wrap && match_member_expr(&mem, vec!["module", "exports"], &self.collect.decls) {
                  self.self_references.insert(key.clone());
                  return Expr::Ident(self.get_export_ident(member.span, &key))
                }
              },
              Expr::This(_) => {
                // this.foo -> $id$export$foo
                if self.collect.static_cjs_exports && !self.collect.should_wrap && !self.in_function_scope && !self.collect.is_esm {
                  self.self_references.insert(key.clone());
                  return Expr::Ident(self.get_export_ident(member.span, &key))
                }
              },
              _ => {}
            }
          },
          _ => {}
        }
      },
      Expr::Call(call) => {
        // require('foo') -> $id$import$foo
        if let Some(source) = match_require(&node, &self.collect.decls, self.collect.ignore_mark) {
          self.add_require(&source);
          return Expr::Ident(self.get_import_ident(call.span, &source, &("*".into()), SourceLocation::from(&self.collect.source_map, call.span)))
        }

        if let Some(source) = match_import(&node, self.collect.ignore_mark) {
          self.add_require(&source);
          let name: JsWord = format!("${}$importAsync${:x}", self.module_id, hash!(source)).into();
          self.dynamic_imports.insert(name.clone(), source.clone());
          if self.collect.non_static_requires.contains(&source) || self.collect.should_wrap {
            self.imported_symbols.insert(name.clone(), (source.clone(), "*".into(), SourceLocation::from(&self.collect.source_map, call.span)));
          }
          return Expr::Ident(Ident::new(name, call.span))
        }
      },
      Expr::This(this) => {
        if !self.in_function_scope {
          // If ESM, replace `this` with `undefined`, otherwise with the CJS exports object.
          if self.collect.is_esm {
            return Expr::Ident(Ident::new("undefined".into(), DUMMY_SP));
          } else if !self.collect.should_wrap {
            self.self_references.insert("*".into());
            return Expr::Ident(self.get_export_ident(this.span, &"*".into()));  
          }
        }
      },
      Expr::Unary(unary) => {
        // typeof require -> "function"
        if unary.op == UnaryOp::TypeOf {
          match &*unary.arg {
            Expr::Ident(ident) => {
              if ident.sym == js_word!("require") && !self.collect.decls.contains(&id!(ident)) {
                return Expr::Lit(Lit::Str(Str {
                  kind: StrKind::Synthesized,
                  has_escape: false,
                  span: unary.span,
                  value: js_word!("function")
                }))
              }
            },
            _ => {}
          }
        }
      }
      _ => {}
    }

    let res = node.fold_children_with(self);
    res
  }

  fn fold_seq_expr(&mut self, node: SeqExpr) -> SeqExpr {
    // This is a hack to work around the SWC fixer pass removing identifiers in sequence expressions
    // that aren't at the end. In general this makes sense, but we need to preserve these so that they
    // can be replaced with a parcelRequire call in the linker. We just wrap with a unary expression to 
    // get around this for now.
    let len = node.exprs.len();
    let exprs = node.exprs.into_iter().enumerate().map(|(i, expr)| {
      if i != len - 1 {
        if let Some(_) = match_require(&*expr, &self.collect.decls, self.collect.ignore_mark) {
          return Box::new(Expr::Unary(UnaryExpr {
            op: UnaryOp::Bang,
            arg: expr.fold_with(self),
            span: DUMMY_SP
          }))
        }
      }

      expr.fold_with(self)
    }).collect();

    SeqExpr {
      exprs,
      ..node
    }
  }

  fn fold_ident(&mut self, node: Ident) -> Ident {
    // import {x} from 'y'; OR const {x} = require('y');
    // x -> $id$import$y$x
    //
    // import * as x from 'y'; OR const x = require('y');
    // x -> $id$import$y
    if let Some((source, local, is_async, loc)) = self.collect.imports.get(&id!(node)) {
      // If the require is accessed in a way we cannot analyze, do not replace.
      // e.g. const {x: {y: z}} = require('x');
      if !self.collect.non_static_requires.contains(source) {
        if *is_async {
          if local != "*" {
            let name: JsWord = format!("${}$importAsync${:x}${:x}", self.module_id, hash!(source), hash!(local)).into();
            self.imported_symbols.insert(name, (source.clone(), local.clone(), loc.clone()));
          } else if self.collect.non_static_access.contains(&id!(node)) {
            let name: JsWord = format!("${}$importAsync${:x}", self.module_id, hash!(source)).into();
            self.imported_symbols.insert(name, (source.clone(), "*".into(), loc.clone()));
          }
        } else {
          return self.get_import_ident(node.span, source, local, loc.clone());
        }
      }
    }

    if let Some(exported) = self.collect.exports.get(&id!(node)) {
      // If wrapped, mark the original symbol as exported.
      // Otherwise replace with an export identifier.
      if self.collect.should_wrap {
        self.exported_symbols.entry(exported.clone()).or_insert((node.sym.clone(), SourceLocation::from(&self.collect.source_map, node.span)));
        return node
      } else {
        return self.get_export_ident(node.span, exported);
      }
    }

    let exports: JsWord = "exports".into();
    if node.sym == exports && !self.collect.decls.contains(&id!(node)) && !self.collect.should_wrap {
      self.self_references.insert("*".into());
      return self.get_export_ident(node.span, &"*".into());
    }

    if node.sym == js_word!("global") && !self.collect.decls.contains(&id!(node)) {
      return Ident::new("$parcel$global".into(), node.span);
    }
    
    if node.span.ctxt() == self.global_ctxt && self.collect.decls.contains(&id!(node)) && !self.collect.should_wrap {
      let new_name: JsWord = format!("${}$var${}", self.module_id, node.sym).into();
      return Ident::new(new_name, node.span)
    }

    node
  }

  fn fold_assign_expr(&mut self, node: AssignExpr) -> AssignExpr {
    if self.collect.should_wrap {
      return node.fold_children_with(self)
    }

    let expr = match &node.left {
      PatOrExpr::Expr(expr) => expr,
      PatOrExpr::Pat(pat) => {
        match &**pat {
          Pat::Expr(expr) => expr,
          _ => return node.fold_children_with(self)
        }
      }
    };

    match &**expr {
      Expr::Member(member) => {
        if match_member_expr(&member, vec!["module", "exports"], &self.collect.decls) {
          let mut assign = node.clone();
          let ident = BindingIdent::from(self.get_export_ident(member.span, &"*".into()));
          assign.left = PatOrExpr::Pat(Box::new(Pat::Ident(ident.clone())));
          assign.right = node.right.fold_with(self);
          return assign
        }

        let is_cjs_exports = match &member.obj {
          ExprOrSuper::Expr(expr) => {
            match &**expr {
              Expr::Member(member) => {
                match_member_expr(&member, vec!["module", "exports"], &self.collect.decls)
              },
              Expr::Ident(ident) => {
                let exports: JsWord = "exports".into();
                ident.sym == exports && !self.collect.decls.contains(&id!(ident))
              },
              _ => false
            }
          },
          _ => false
        };

        if is_cjs_exports {
          let key: JsWord = if self.collect.static_cjs_exports {
            match &*member.prop {
              Expr::Ident(ident) => {
                if !member.computed {
                  ident.sym.clone()
                } else {
                  unreachable!("Unexpected non-static CJS export");
                }
              },
              Expr::Lit(lit) => {
                match lit {
                  Lit::Str(str_) => str_.value.clone(),
                  _ => unreachable!("Unexpected non-static CJS export")
                }
              },
              _ => unreachable!("Unexpected non-static CJS export")
            }
          } else {
            "*".into()
          };

          let ident = BindingIdent::from(self.get_export_ident(member.span, &key));
          if self.collect.static_cjs_exports {
            self.export_decls.insert(ident.id.sym.clone());
          }

          let mut assign = node.clone();
          assign.left = if self.collect.static_cjs_exports {
            PatOrExpr::Pat(Box::new(Pat::Ident(ident.clone())))
          } else {
            let mut member = member.clone();
            member.obj = ExprOrSuper::Expr(Box::new(Expr::Ident(ident.id.clone())));
            member.prop = member.prop.fold_with(self);
            PatOrExpr::Pat(Box::new(Pat::Expr(Box::new(Expr::Member(member)))))
          };
          assign.right = node.right.fold_with(self);
          return assign
        }
      },
      _ => {}
    }

    node.fold_children_with(self)
  }

  fn fold_prop(&mut self, node: Prop) -> Prop {
    if self.collect.should_wrap {
      return node.fold_children_with(self)
    }
    
    // TODO: test
    match node {
      Prop::Shorthand(ident) => {
        Prop::KeyValue(KeyValueProp {
          key: PropName::Ident(Ident::new(ident.sym.clone(), DUMMY_SP)),
          value: Box::new(Expr::Ident(ident.fold_with(self)))
        })
      },
      Prop::KeyValue(kv) => {
        let mut kv = kv.clone();
        kv.value = kv.value.fold_with(self);
        Prop::KeyValue(kv)
      },
      Prop::Getter(getter) => {
        let mut getter = getter.clone();
        getter.body = getter.body.fold_with(self);
        Prop::Getter(getter)
      },
      Prop::Setter(setter) => {
        let mut setter = setter.clone();
        setter.body = setter.body.fold_with(self);
        Prop::Setter(setter)
      },
      Prop::Method(method) => {
        let mut method = method.clone();
        method.function = method.function.fold_with(self);
        Prop::Method(method)
      },
      _ => node.fold_children_with(self)
    }
  }

  fn fold_object_pat_prop(&mut self, node: ObjectPatProp) -> ObjectPatProp {
    if self.collect.should_wrap {
      return node.fold_children_with(self)
    }
    
    // var {a, b} = foo; -> var {a: $id$var$a, b: $id$var$b} = foo;
    match &node {
      ObjectPatProp::Assign(assign) => {
        ObjectPatProp::KeyValue(KeyValuePatProp {
          key: PropName::Ident(Ident::new(assign.key.sym.clone(), DUMMY_SP)),
          value: Box::new(match &assign.value {
            Some(value) => {
              Pat::Assign(AssignPat {
                left: Box::new(Pat::Ident(BindingIdent::from(assign.key.clone().fold_with(self)))),
                right: value.clone().fold_with(self),
                span: DUMMY_SP,
                type_ann: None
              })
            },
            None => {
              Pat::Ident(BindingIdent::from(assign.key.clone().fold_with(self)))
            }
          })
        })
      },
      _ => node.fold_children_with(self)
    }
  }
}

impl<'a> Hoist<'a> {
  fn add_require(&mut self, source: &JsWord) {
    self.requires_in_stmt.push(ModuleItem::ModuleDecl(ModuleDecl::Import(ImportDecl {
      specifiers: vec![],
      asserts: None,
      span: DUMMY_SP,
      src: Str { value: format!("{}:{}", self.module_id, source).into(), span: DUMMY_SP, kind: StrKind::Synthesized, has_escape: false },
      type_only: false
    })));
  }

  fn get_import_ident(&mut self, span: swc_common::Span, source: &JsWord, local: &JsWord, loc: SourceLocation) -> Ident {
    let new_name: JsWord = if local == "*" {
      format!("${}$import${:x}", self.module_id, hash!(source)).into()
    } else {
      format!("${}$import${:x}${:x}", self.module_id, hash!(source), hash!(local)).into()
    };
    self.imported_symbols.insert(new_name.clone(), (source.clone(), local.clone(), loc));
    return Ident::new(new_name, span)
  }

  fn get_export_ident(&mut self, span: swc_common::Span, exported: &JsWord) -> Ident {
    let new_name: JsWord = if exported == "*" {
      format!("${}$exports", self.module_id).into()
    } else {
      format!("${}$export${}", self.module_id, exported).into()
    };

    self.exported_symbols.entry(exported.clone()).or_insert((new_name.clone(), SourceLocation::from(&self.collect.source_map, span)));
    
    let mut span = span;
    span.ctxt = SyntaxContext::empty();
    return Ident::new(new_name, span)
  }
}

macro_rules! visit_fn {
  ($self: ident, $node: ident) => {
    let in_module_this = $self.in_module_this;
    let in_function = $self.in_function;
    $self.in_module_this = false;
    $self.in_function = true;
    $node.visit_children_with($self);
    $self.in_module_this = in_module_this;
    $self.in_function = in_function;
  };
}

pub struct Collect {
  source_map: Lrc<swc_common::SourceMap>,
  pub decls: HashSet<IdentId>,
  ignore_mark: Mark,
  static_cjs_exports: bool,
  has_cjs_exports: bool,
  is_esm: bool,
  should_wrap: bool,
  pub imports: HashMap<IdentId, (JsWord, JsWord, bool, SourceLocation)>,
  exports: HashMap<IdentId, JsWord>,
  non_static_access: HashSet<IdentId>,
  non_static_requires: HashSet<JsWord>,
  wrapped_requires: HashSet<JsWord>,
  in_module_this: bool,
  in_top_level: bool,
  in_export_decl: bool,
  in_function: bool,
}

impl Collect {
  pub fn new(source_map: Lrc<swc_common::SourceMap>, decls: HashSet<IdentId>, ignore_mark: Mark) -> Self {
    Collect {
      source_map,
      decls,
      ignore_mark,
      static_cjs_exports: true,
      has_cjs_exports: false,
      is_esm: false,
      should_wrap: false,
      imports: HashMap::new(),
      exports: HashMap::new(),
      non_static_access: HashSet::new(),
      non_static_requires: HashSet::new(),
      wrapped_requires: HashSet::new(),
      in_module_this: true,
      in_top_level: true,
      in_export_decl: false,
      in_function: false
    }
  }
}

impl Visit for Collect {
  fn visit_module(&mut self, node: &Module, _parent: &dyn Node) {
    self.in_module_this = true;
    self.in_top_level = true;
    self.in_function = false;
    node.visit_children_with(self);
    self.in_module_this = false;
  }

  fn visit_function(&mut self, node: &Function, _parent: &dyn Node) {
    visit_fn!(self, node);
  }

  fn visit_class(&mut self, node: &Class, _parent: &dyn Node) {
    visit_fn!(self, node);
  }

  fn visit_arrow_expr(&mut self, node: &ArrowExpr, _parent: &dyn Node) {
    let in_function = self.in_function;
    self.in_function = true;
    node.visit_children_with(self);
    self.in_function = in_function;
  }

  fn visit_module_item(&mut self, node: &ModuleItem, _parent: &dyn Node) {
    match node {
      ModuleItem::ModuleDecl(_decl) => {
        self.is_esm = true;
      },
      ModuleItem::Stmt(stmt) => {
        match stmt {
          Stmt::Decl(decl) => {
            match decl {
              Decl::Var(_var) => {
                decl.visit_children_with(self);
                return
              },
              _ => {}
            }
          },
          Stmt::Expr(expr) => {
            // Top-level require(). Do not traverse further so it is not marked as wrapped.
            if let Some(_source) = self.match_require(&*expr.expr) {
              return
            }

            // TODO: handle require('foo').bar / require('foo').bar()
          },
          _ => {}
        }
      }
    }

    self.in_top_level = false;
    node.visit_children_with(self);
    self.in_top_level = true;
  }

  fn visit_import_decl(&mut self, node: &ImportDecl, _parent: &dyn Node) {
    for specifier in &node.specifiers {
      match specifier {
        ImportSpecifier::Named(named) => {
          let imported = match &named.imported {
            Some(imported) => imported.sym.clone(),
            None => named.local.sym.clone()
          };
          self.imports.insert(id!(named.local), (node.src.value.clone(), imported, false, SourceLocation::from(&self.source_map, named.span)));
        },
        ImportSpecifier::Default(default) => {
          self.imports.insert(id!(default.local), (node.src.value.clone(), js_word!("default"), false, SourceLocation::from(&self.source_map, default.span)));
        },
        ImportSpecifier::Namespace(namespace) => {
          self.imports.insert(id!(namespace.local), (node.src.value.clone(), "*".into(), false, SourceLocation::from(&self.source_map, namespace.span)));
        }
      }
    }
  }

  fn visit_named_export(&mut self, node: &NamedExport, _parent: &dyn Node) {
    if node.src.is_some() {
      return
    }

    for specifier in &node.specifiers {
      match specifier {
        ExportSpecifier::Named(named) => {
          let exported = match &named.exported {
            Some(exported) => exported.sym.clone(),
            None => named.orig.sym.clone()
          };
          self.exports.entry(id!(named.orig)).or_insert(exported);
        },
        ExportSpecifier::Default(default) => {
          self.exports.entry(id!(default.exported)).or_insert(js_word!("default"));
        },
        ExportSpecifier::Namespace(namespace) => {
          self.exports.entry(id!(namespace.name)).or_insert("*".into());
        }
      }
    }
  }

  fn visit_export_decl(&mut self, node: &ExportDecl, _parent: &dyn Node) {
    match &node.decl {
      Decl::Class(class) => {
        self.exports.insert(id!(class.ident), class.ident.sym.clone());
      },
      Decl::Fn(func) => {
        self.exports.insert(id!(func.ident), func.ident.sym.clone());
      },
      Decl::Var(var) => {
        for decl in &var.decls {
          self.in_export_decl = true;
          decl.name.visit_with(decl, self);
          self.in_export_decl = false;

          decl.init.visit_with(decl, self);
        }
      }
      _ => {}
    }

    node.visit_children_with(self);
  }

  fn visit_export_default_decl(&mut self, node: &ExportDefaultDecl, _parent: &dyn Node) {
    match &node.decl {
      DefaultDecl::Class(class) => {
        if let Some(ident) = &class.ident {
          self.exports.insert(id!(ident), "default".into());
        }
      },
      DefaultDecl::Fn(func) => {
        if let Some(ident) = &func.ident {
          self.exports.insert(id!(ident), "default".into());
        }
      },
      _ => {
        unreachable!("unsupported export default declaration");
      }
    };

    node.visit_children_with(self);
  }

  fn visit_return_stmt(&mut self, node: &ReturnStmt, _parent: &dyn Node) {
    if !self.in_function {
      self.should_wrap = true;
    }

    node.visit_children_with(self)
  }

  fn visit_binding_ident(&mut self, node: &BindingIdent, _parent: &dyn Node) {
    if self.in_export_decl {
      self.exports.insert(id!(node.id), node.id.sym.clone());
    }
  }

  fn visit_assign_pat_prop(&mut self, node: &AssignPatProp, _parent: &dyn Node) {
    if self.in_export_decl {
      self.exports.insert(id!(node.key), node.key.sym.clone());
    }
  }

  fn visit_member_expr(&mut self, node: &MemberExpr, _parent: &dyn Node) {
    // if module.exports, ensure only assignment or static member expression
    // if exports, ensure only static member expression
    // if require, could be static access (handle in fold)

    if match_member_expr(&node, vec!["module", "exports"], &self.decls) {
      self.static_cjs_exports = false;
      self.has_cjs_exports = true;
      return
    }

    let is_static = match &*node.prop {
      Expr::Ident(_) => !node.computed,
      Expr::Lit(lit) => {
        match lit {
          Lit::Str(_) => true,
          _ => false
        }
      },
      _ => false
    };

    match &node.obj {
      ExprOrSuper::Expr(expr) => {
        match &**expr {
          Expr::Member(member) => {
            if match_member_expr(&member, vec!["module", "exports"], &self.decls) {
              self.has_cjs_exports = true;
              if !is_static {
                self.static_cjs_exports = false;
              }
            }
            return
          },
          Expr::Ident(ident) => {
            let exports: JsWord = "exports".into();
            if ident.sym == exports && !self.decls.contains(&id!(ident)) {
              self.has_cjs_exports = true;
              if !is_static {
                self.static_cjs_exports = false
              }
            }

            // `import` isn't really an identifier...
            if !is_static && ident.sym != js_word!("import") {
              self.non_static_access.insert(id!(ident));
            }
            return
          },
          Expr::This(_this) => {
            if self.in_module_this {
              self.has_cjs_exports = true;
              if !is_static {
                self.static_cjs_exports = false;
              }
            }
            return
          },
          _ => {}
        }
      },
      _ => {}
    }

    node.visit_children_with(self);
  }

  fn visit_expr(&mut self, node: &Expr, _parent: &dyn Node) {
    match node {
      Expr::Ident(ident) => {
        // Bail if `module` or `exports` are accessed non-statically.
        let is_module = ident.sym == js_word!("module");
        let exports: JsWord = "exports".into();
        let is_exports = ident.sym == exports;
        if (is_module || is_exports) && !self.decls.contains(&id!(ident)) {
          self.has_cjs_exports = true;
          self.static_cjs_exports = false;
          if is_module {
            self.should_wrap = true;
          }
        }

        // `import` isn't really an identifier...
        if ident.sym != js_word!("import") {
          self.non_static_access.insert(id!(ident));
        }
      },
      _ => {
        node.visit_children_with(self);
      }
    }
  }

  fn visit_this_expr(&mut self, _node: &ThisExpr, _parent: &dyn Node) {
    if self.in_module_this {
      self.has_cjs_exports = true;
      self.static_cjs_exports = false;
    }
  }

  fn visit_assign_expr(&mut self, node: &AssignExpr, _parent: &dyn Node) {
    // if rhs is a require, record static accesses
    // if lhs is `exports`, mark as CJS exports re-assigned
    // if lhs is `module.exports`
    // if lhs is `module.exports.XXX` or `exports.XXX`, record static export

    node.visit_children_with(self);

    match &node.left {
      PatOrExpr::Pat(pat) => {
        if has_binding_identifier(pat, &"exports".into(), &self.decls) {
          // Must wrap. https://parcel2-repl.now.sh/#JTdCJTIyY3VycmVudFByZXNldCUyMiUzQSUyMkphdmFzY3JpcHQlMjIlMkMlMjJvcHRpb25zJTIyJTNBJTdCJTIybWluaWZ5JTIyJTNBZmFsc2UlMkMlMjJzY29wZUhvaXN0JTIyJTNBdHJ1ZSUyQyUyMnNvdXJjZU1hcHMlMjIlM0FmYWxzZSUyQyUyMnB1YmxpY1VybCUyMiUzQSUyMiUyRl9fcmVwbF9kaXN0JTIyJTJDJTIydGFyZ2V0VHlwZSUyMiUzQSUyMmJyb3dzZXJzJTIyJTJDJTIydGFyZ2V0RW52JTIyJTNBbnVsbCUyQyUyMm91dHB1dEZvcm1hdCUyMiUzQW51bGwlMkMlMjJobXIlMjIlM0FmYWxzZSUyQyUyMm1vZGUlMjIlM0ElMjJwcm9kdWN0aW9uJTIyJTJDJTIycmVuZGVyR3JhcGhzJTIyJTNBZmFsc2UlMkMlMjJ2aWV3U291cmNlbWFwcyUyMiUzQWZhbHNlJTJDJTIyZGVwZW5kZW5jaWVzJTIyJTNBJTVCJTVEJTdEJTJDJTIyYXNzZXRzJTIyJTNBJTVCJTVCJTIyc3JjJTJGaW5kZXguanMlMjIlMkMlMjJmdW5jdGlvbiUyMGxvZ0V4cG9ydHMoKSUyMCU3QiU1Q24lMjAlMjBjb25zb2xlLmxvZyhleHBvcnRzKSUzQiU1Q24lN0QlNUNuZXhwb3J0cy50ZXN0JTIwJTNEJTIwMiUzQiU1Q25sb2dFeHBvcnRzKCklM0IlNUNuZXhwb3J0cyUyMCUzRCUyMCU3QnRlc3QlM0ElMjA0JTdEJTNCJTVDbmxvZ0V4cG9ydHMoKSUzQiUyMiUyQzElNUQlMkMlNUIlMjJzcmMlMkZvdGhlci5qcyUyMiUyQyUyMmNsYXNzJTIwVGhpbmclMjAlN0IlNUNuJTIwJTIwcnVuKCklMjAlN0IlNUNuJTIwJTIwJTIwJTIwY29uc29sZS5sb2coJTVDJTIyVGVzdCU1QyUyMiklM0IlNUNuJTIwJTIwJTdEJTIwJTVDbiU3RCU1Q24lNUNuY29uc3QlMjB4JTIwJTNEJTIwMTIzJTNCJTVDbmV4cG9ydCUyMCU3QlRoaW5nJTJDJTIweCU3RCUzQiUyMiU1RCU1RCU3RA==
          self.static_cjs_exports = false;
          self.has_cjs_exports = true;
          self.should_wrap = true;
        }
      },
      _ => {}
    }
  }

  fn visit_var_declarator(&mut self, node: &VarDeclarator, _parent: &dyn Node) {
    // if init is a require call, record static accesses
    if let Some(init) = &node.init {
      if let Some(source) = self.match_require(init) {
        self.add_pat_imports(&node.name, &source, false);
        return;
      }

      match &**init {
        Expr::Member(member) => {
          match &member.obj {
            ExprOrSuper::Expr(expr) => {
              if let Some(source) = self.match_require(&*expr) {
                // Convert member expression on require to a destructuring assignment.
                // const yx = require('y').x; -> const {x: yx} = require('x');
                let key = match &*member.prop {
                  Expr::Ident(ident) => {
                    if !member.computed {
                      PropName::Ident(ident.clone())
                    } else {
                      PropName::Computed(ComputedPropName { span: DUMMY_SP, expr: Box::new(*expr.clone()) })
                    }
                  },
                  Expr::Lit(lit) => {
                    match lit {
                      Lit::Str(str_) => PropName::Str(str_.clone()),
                      _ => PropName::Computed(ComputedPropName { span: DUMMY_SP, expr: Box::new(*expr.clone()) })
                    }
                  },
                  _ => PropName::Computed(ComputedPropName { span: DUMMY_SP, expr: Box::new(*expr.clone()) })
                };

                self.add_pat_imports(&Pat::Object(ObjectPat {
                  optional: false,
                  span: DUMMY_SP,
                  type_ann: None,
                  props: vec![ObjectPatProp::KeyValue(KeyValuePatProp {
                    key,
                    value: Box::new(node.name.clone())
                  })]
                }), &source, false);
                return
              }    
            },
            _ => {}
          }
        },
        Expr::Await(await_exp) => {
          // let x = await import('foo');
          // let {x} = await import('foo');
          if let Some(source) = match_import(&*await_exp.arg, self.ignore_mark) {
            self.add_pat_imports(&node.name, &source, true);
            return
          }
        },
        _ => {}
      }
    }

    node.visit_children_with(self);
  }

  fn visit_call_expr(&mut self, node: &CallExpr, _parent: &dyn Node) {
    // If we reached this visitor, this is a non-top-level require that isn't in a variable
    // declaration. We need to wrap the referenced module to preserve side effect ordering.
    if let Some(source) = self.match_require(&Expr::Call(node.clone())) {
      self.wrapped_requires.insert(source.clone());
    }

    if let Some(source) = match_import(&Expr::Call(node.clone()), self.ignore_mark) {
      self.non_static_requires.insert(source.clone());
      self.wrapped_requires.insert(source.clone());
    }
    
    match &node.callee {
      ExprOrSuper::Expr(expr) => {
        match &**expr {
          Expr::Ident(ident) => {
            if ident.sym == js_word!("eval") && !self.decls.contains(&id!(ident)) {
              self.should_wrap = true
            }
          },
          Expr::Member(member) => {
            // import('foo').then(foo => ...);
            if let ExprOrSuper::Expr(obj) = &member.obj {
              if let Some(source) = match_import(&*obj, self.ignore_mark) {
                let then: JsWord = "then".into();
                let is_then = match &*member.prop {
                  Expr::Ident(ident) => !member.computed && ident.sym == then,
                  Expr::Lit(lit) => {
                    match lit {
                      Lit::Str(str) => str.value == then,
                      _ => false
                    }
                  },
                  _ => false
                };

                if is_then {
                  match node.args.get(0) {
                    Some(ExprOrSpread { expr, .. }) => {
                      let param = match &**expr {
                        Expr::Fn(func) => match func.function.params.get(0) {
                          Some(param) => Some(&param.pat),
                          None => None
                        },
                        Expr::Arrow(arrow) => arrow.params.get(0),
                        _ => None
                      };

                      if let Some(param) = param {
                        self.add_pat_imports(param, &source, true);
                      } else {
                        self.non_static_requires.insert(source.clone());
                        self.wrapped_requires.insert(source.clone());
                      }

                      expr.visit_with(node, self);
                      return
                    },
                    _ => {}
                  }
                }
              }
            }
          }
          _ => {}
        }
      },
      _ => {}
    }

    node.visit_children_with(self);
  }
}

impl Collect {
  pub fn match_require(&self, node: &Expr) -> Option<JsWord> {
    match_require(node, &self.decls, self.ignore_mark)
  }

  fn add_pat_imports(&mut self, node: &Pat, src: &JsWord, is_import: bool) {
    if !self.in_top_level {
      self.wrapped_requires.insert(src.clone());
      if !is_import {
        self.non_static_requires.insert(src.clone());
      }
    }

    match node {
      Pat::Ident(ident) => {
        // let x = require('y');
        // Need to track member accesses of `x`.
        self.imports.insert(id!(ident.id), (src.clone(), "*".into(), is_import, SourceLocation::from(&self.source_map, ident.id.span)));
      },
      Pat::Object(object) => {
        for prop in &object.props {
          match prop {
            ObjectPatProp::KeyValue(kv) => {
              let local = match &kv.key {
                PropName::Ident(ident) => ident.sym.clone(),
                PropName::Str(str) => str.value.clone(),
                _ => {
                  // Non-static. E.g. computed property.
                  self.non_static_requires.insert(src.clone());
                  continue;
                }
              };

              match &*kv.value {
                Pat::Ident(ident) => {
                  // let {x: y} = require('y');
                  // Need to track `y` as a used symbol.
                  self.imports.insert(id!(ident.id), (src.clone(), local, is_import, SourceLocation::from(&self.source_map, ident.id.span)));
                },
                _ => {
                  // Non-static.
                  self.non_static_requires.insert(src.clone());
                }
              }
            },
            ObjectPatProp::Assign(assign) => {
              // let {x} = require('y');
              // let {x = 2} = require('y');
              // Need to track `x` as a used symbol.
              self.imports.insert(id!(assign.key), (src.clone(), assign.key.sym.clone(), is_import, SourceLocation::from(&self.source_map, assign.key.span)));
            },
            ObjectPatProp::Rest(_rest) => {
              // let {x, ...y} = require('y');
              // Non-static. We don't know what keys are used.
              self.non_static_requires.insert(src.clone());
            }
          }
        }
      },
      _ => {
        // Non-static.
        self.non_static_requires.insert(src.clone());
      }
    }
  }
}

fn is_marked(span: swc_common::Span, mark: Mark) -> bool {
  let mut ctxt = span.ctxt().clone();

  loop {
    let m = ctxt.remove_mark();
    if m == Mark::root() {
      return false;
    }

    if m == mark {
      return true;
    }
  }
}

fn match_require(node: &Expr, decls: &HashSet<IdentId>, ignore_mark: Mark) -> Option<JsWord> {
  match node {
    Expr::Call(call) => {
      match &call.callee {
        ExprOrSuper::Expr(expr) => {
          match &**expr {
            Expr::Ident(ident) => {
              if ident.sym == js_word!("require") && !decls.contains(&id!(ident)) && !is_marked(ident.span, ignore_mark) {
                if let Some(arg) = call.args.get(0) {
                  if let Expr::Lit(lit) = &*arg.expr {
                    if let Lit::Str(str_) = lit {
                      return Some(str_.value.clone())
                    }
                  }
                }
              }

              None
            },
            _ => None
          }
        },
        _ => None
      }
    },
    _ => None
  }
}

fn match_import(node: &Expr, ignore_mark: Mark) -> Option<JsWord> {
  match node {
    Expr::Call(call) => {
      match &call.callee {
        ExprOrSuper::Expr(expr) => {
          match &**expr {
            Expr::Ident(ident) => {
              if ident.sym == js_word!("import") && !is_marked(ident.span, ignore_mark) {
                if let Some(arg) = call.args.get(0) {
                  if let Expr::Lit(lit) = &*arg.expr {
                    if let Lit::Str(str_) = lit {
                      return Some(str_.value.clone())
                    }
                  }
                }
              }

              None
            },
            _ => None
          }
        },
        _ => None
      }
    },
    _ => None
  }
}

fn has_binding_identifier(node: &Pat, sym: &JsWord, decls: &HashSet<IdentId>) -> bool {
  match node {
    Pat::Ident(ident) => {
      if ident.id.sym == *sym && !decls.contains(&id!(ident.id)) {
        return true
      }
    },
    Pat::Object(object) => {
      for prop in &object.props {
        match prop {
          ObjectPatProp::KeyValue(kv) => {
            if has_binding_identifier(&*kv.value, sym, decls) {
              return true
            }
          },
          ObjectPatProp::Assign(assign) => {
            if assign.key.sym == *sym && !decls.contains(&id!(assign.key)) {
              return true
            }
          },
          ObjectPatProp::Rest(rest) => {
            if has_binding_identifier(&*rest.arg, sym, decls) {
              return true
            }
          }
        }
      }
    },
    Pat::Array(array) => {
      for el in &array.elems {
        if let Some(el) = el {
          if has_binding_identifier(&el, sym, decls) {
            return true
          }
        }
      }
    },
    _ => {}
  }

  false
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::collect_decls;
  use swc_common::comments::SingleThreadedComments;
  use swc_common::{FileName, SourceMap, sync::Lrc, DUMMY_SP, chain, Globals, Mark};
  use swc_ecmascript::parser::lexer::Lexer;
  use swc_ecmascript::parser::{Parser, EsConfig, TsConfig, StringInput, Syntax, PResult};
  use swc_ecmascript::transforms::resolver_with_mark;
  use swc_ecmascript::codegen::text_writer::JsWriter;
  extern crate indoc;
  use self::indoc::indoc;

  fn parse(code: &str) -> (Collect, String, HoistResult) {
    let source_map = Lrc::new(SourceMap::default());
    let source_file = source_map.new_source_file(
      FileName::Anon,
      code.into()
    );
  
    let comments = SingleThreadedComments::default();  
    let mut esconfig = EsConfig::default();
    esconfig.dynamic_import = true;
    let lexer = Lexer::new(
      Syntax::Es(esconfig),
      Default::default(),
      StringInput::from(&*source_file),
      Some(&comments),
    );
  
    let mut parser = Parser::new_from(lexer);
    match parser.parse_module() {
      Ok(module) => {
        swc_common::GLOBALS.set(&Globals::new(), || {
          swc_ecmascript::transforms::helpers::HELPERS.set(&swc_ecmascript::transforms::helpers::Helpers::new(false), || {
            let global_mark = Mark::fresh(Mark::root());
            let module = module.fold_with(&mut resolver_with_mark(global_mark));

            let mut collect = Collect::new(source_map.clone(), collect_decls(&module), Mark::fresh(Mark::root()));
            module.visit_with(&Invalid { span: DUMMY_SP } as _, &mut collect);
            
            let (module, res) = {
              let mut hoist = Hoist::new("abc", &collect, global_mark);
              let module = module.fold_with(&mut hoist);
              (module, hoist.get_result())
            };
            let code = emit(source_map, comments, &module);
            (collect, code, res)
          })
        })
      },
      Err(err) => {
        panic!("{:?}", err);
      }
    }
  }

  fn emit(source_map: Lrc<SourceMap>, comments: SingleThreadedComments, program: &Module) -> String {
    let mut src_map_buf = vec![];
    let mut buf = vec![];
    {
      let writer = Box::new(
        JsWriter::new(
          source_map.clone(),
          "\n",
          &mut buf,
          Some(&mut src_map_buf),
        )
      );
      let config = swc_ecmascript::codegen::Config { minify: false };
      let mut emitter = swc_ecmascript::codegen::Emitter {
        cfg: config,
        comments: Some(&comments),
        cm: source_map.clone(),
        wr: writer,
      };
      
      emitter.emit_module(&program);
    }
  
    return String::from_utf8(buf).unwrap();
  }

  macro_rules! map(
    { $($key:expr => $value:expr),* } => {
      {
        let mut m = HashMap::new();
        $(
          m.insert($key, $value);
        )*
        m
      }
    };
  );

  macro_rules! set(
    { $($key:expr),* } => {
      {
        let mut m = HashSet::new();
        $(
          m.insert($key);
        )*
        m
      }
    };
  );

  macro_rules! w {
    ($s: expr) => {
      {
        let w: JsWord = $s.into();
        w
      }
    };
  }

  macro_rules! assert_eq_imports {
    ($m: expr, $match: expr) => {
      {
        let mut map = HashMap::new();
        for (key, val) in $m {
          map.insert(key.0, (val.0, val.1, val.2));
        }
        assert_eq!(map, $match);
      }
    };
  }

  macro_rules! assert_eq_imported_symbols {
    ($m: expr, $match: expr) => {
      {
        let mut map = HashMap::new();
        for (key, val) in $m {
          map.insert(key, (val.0, val.1));
        }
        assert_eq!(map, $match);
      }
    };
  }

  macro_rules! assert_eq_set {
    ($m: expr, $match: expr) => {
      {
        let mut map = HashSet::new();
        for item in $m {
          map.insert(item.0);
        }
        assert_eq!(map, $match);
      }
    };
  }

  #[test]
  fn esm() {
    let (collect, _code, _hoist) = parse(r#"
    import {foo as bar} from 'other';
    export {bar as test};
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("bar") => (w!("other"), w!("foo"), false) });
  }

  #[test]
  fn cjs_namespace() {
    let (collect, _code, _hoist) = parse(r#"
    const x = require('other');
    console.log(x.foo);
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), false) });
    assert_eq!(collect.non_static_access, set!{});
  }

  #[test]
  fn cjs_namespace_non_static() {
    let (collect, _code, _hoist) = parse(r#"
    const x = require('other');
    console.log(x[foo]);
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), false) });
    assert_eq_set!(collect.non_static_access, set!{ w!("x") });

    let (collect, _code, _hoist) = parse(r#"
    const x = require('other');
    console.log(x);
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), false) });
    assert_eq_set!(collect.non_static_access, set!{ w!("x") });
  }

  #[test]
  fn cjs_destructure() {
    let (collect, _code, _hoist) = parse(r#"
    const {foo: bar} = require('other');
    exports.test = bar;
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("bar") => (w!("other"), w!("foo"), false) });
    assert_eq!(collect.static_cjs_exports, true);
  }

  #[test]
  fn cjs_reassign() {
    let (collect, _code, _hoist) = parse(r#"
    exports = 2;
    "#);
    assert_eq!(collect.should_wrap, true);
  }

  #[test]
  fn cjs_non_static_exports() {
    let (collect, _code, _hoist) = parse(r#"
    exports[test] = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, false);

    let (collect, _code, _hoist) = parse(r#"
    module.exports[test] = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, false);

    let (collect, _code, _hoist) = parse(r#"
    this[test] = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, false);

    let (collect, _code, _hoist) = parse(r#"
    module.exports[test] = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, false);

    let (collect, _code, _hoist) = parse(r#"
    alert(exports)
    "#);
    assert_eq!(collect.static_cjs_exports, false);

    let (collect, _code, _hoist) = parse(r#"
    alert(module.exports)
    "#);
    assert_eq!(collect.static_cjs_exports, false);

    let (collect, _code, _hoist) = parse(r#"
    alert(this)
    "#);
    assert_eq!(collect.static_cjs_exports, false);

    let (collect, _code, _hoist) = parse(r#"
    exports.foo = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, true);

    let (collect, _code, _hoist) = parse(r#"
    module.exports.foo = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, true);

    let (collect, _code, _hoist) = parse(r#"
    this.foo = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, true);

    let (collect, _code, _hoist) = parse(r#"
    var exports = {};
    exports[foo] = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, true);

    let (collect, _code, _hoist) = parse(r#"
    var module = {exports: {}};
    module.exports[foo] = 2;
    "#);
    assert_eq!(collect.static_cjs_exports, true);

    let (collect, _code, _hoist) = parse(r#"
    test(function(exports) { return Object.keys(exports) })
    "#);
    assert_eq!(collect.static_cjs_exports, true);

    let (collect, _code, _hoist) = parse(r#"
    test(exports => Object.keys(exports))
    "#);
    assert_eq!(collect.static_cjs_exports, true);
  }

  #[test]
  fn dynamic_import() {
    let (collect, _code, _hoist) = parse(r#"
    async function test() {
      const x = await import('other');
      x.foo;
    }
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), true) });
    assert_eq_set!(collect.non_static_access, set!{});
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    async function test() {
      const x = await import('other');
      x[foo];
    }
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), true) });
    assert_eq_set!(collect.non_static_access, set!{ w!("x") });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    async function test() {
      const {foo} = await import('other');
    }
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("foo") => (w!("other"), w!("foo"), true) });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    async function test() {
      const {foo: bar} = await import('other');
    }
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("bar") => (w!("other"), w!("foo"), true) });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other').then(x => x.foo);
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), true) });
    assert_eq_set!(collect.non_static_access, set!{});
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other').then(x => x);
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), true) });
    assert_eq_set!(collect.non_static_access, set!{ w!("x") });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other').then(({foo}) => foo);
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("foo") => (w!("other"), w!("foo"), true) });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other').then(({foo: bar}) => bar);
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("bar") => (w!("other"), w!("foo"), true) });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other').then(function (x) { return x.foo });
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), true) });
    assert_eq_set!(collect.non_static_access, set!{});
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other').then(function (x) { return x });
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("x") => (w!("other"), w!("*"), true) });
    assert_eq_set!(collect.non_static_access, set!{ w!("x") });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other').then(function ({foo}) {});
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("foo") => (w!("other"), w!("foo"), true) });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other').then(function ({foo: bar}) {});
    "#);
    assert_eq_imports!(collect.imports, map!{ w!("bar") => (w!("other"), w!("foo"), true) });
    assert_eq!(collect.non_static_requires, set!{});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    import('other');
    "#);
    assert_eq_imports!(collect.imports, map!{});
    assert_eq!(collect.non_static_requires, set!{w!("other")});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    let other = import('other');
    "#);
    assert_eq_imports!(collect.imports, map!{});
    assert_eq!(collect.non_static_requires, set!{w!("other")});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});

    let (collect, _code, _hoist) = parse(r#"
    async function test() {
      let {...other} = await import('other');
    }
    "#);
    assert_eq_imports!(collect.imports, map!{});
    assert_eq!(collect.non_static_requires, set!{w!("other")});
    assert_eq!(collect.wrapped_requires, set!{w!("other")});
  }

  #[test]
  fn fold_import() {
    let (_collect, code, _hoist) = parse(r#"
    import {foo as bar} from 'other';
    let test = {bar: 3};
    console.log(bar, test.bar);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    let $abc$var$test = {
        bar: 3
    };
    console.log($abc$import$558d6cfb8af8a010$ba02ad2230917043, $abc$var$test.bar);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    import * as foo from 'other';
    console.log(foo.bar);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    console.log($abc$import$558d6cfb8af8a010$3705fc5f2281438d);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    import other from 'other';
    console.log(other, other.bar);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    console.log($abc$import$558d6cfb8af8a010$9099ad97b570f7c, $abc$import$558d6cfb8af8a010$9099ad97b570f7c.bar);
    "#});
  }

  #[test]
  fn fold_import_hoist() {
    let (_collect, code, _hoist) = parse(r#"
    import foo from 'other';
    console.log(foo);
    import bar from 'bar';
    console.log(bar);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    import   "abc:bar";
    console.log($abc$import$558d6cfb8af8a010$9099ad97b570f7c);
    console.log($abc$import$3705fc5f2281438d$9099ad97b570f7c);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    import foo from 'other';
    console.log(foo);
    const x = require('x');
    console.log(x);
    import bar from 'bar';
    console.log(bar);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    import   "abc:bar";
    console.log($abc$import$558d6cfb8af8a010$9099ad97b570f7c);
    import   "abc:x";
    console.log($abc$import$10b1f2ceae7ab64e);
    console.log($abc$import$3705fc5f2281438d$9099ad97b570f7c);
    "#});
  }

  #[test]
  fn fold_static_require() {
    let (_collect, code, _hoist) = parse(r#"
    const x = 4, {bar} = require('other'), baz = 3;
    console.log(bar);
    "#);

    assert_eq!(code, indoc!{r#"
    const $abc$var$x = 4;
    import   "abc:other";
    const $abc$var$baz = 3;
    console.log($abc$import$558d6cfb8af8a010$3705fc5f2281438d);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    const x = 3, foo = require('other'), bar = 2;
    console.log(foo.bar);
    "#);

    assert_eq!(code, indoc!{r#"
    const $abc$var$x = 3;
    import   "abc:other";
    const $abc$var$bar = 2;
    console.log($abc$import$558d6cfb8af8a010$3705fc5f2281438d);
    "#});
  }

  #[test]
  fn fold_non_static_require() {
    let (_collect, code, _hoist) = parse(r#"
    const {foo, ...bar} = require('other');
    console.log(foo, bar);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    const { foo: $abc$var$foo , ...$abc$var$bar } = $abc$import$558d6cfb8af8a010;
    console.log($abc$var$foo, $abc$var$bar);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    const {x: {y: z}} = require('x');
    console.log(z);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:x";
    const { x: { y: $abc$var$z  }  } = $abc$import$10b1f2ceae7ab64e;
    console.log($abc$var$z);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    const foo = require('other');
    console.log(foo[bar]);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    console.log($abc$import$558d6cfb8af8a010[bar]);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    const foo = require('other');
    console.log(foo[bar], foo.baz);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    console.log($abc$import$558d6cfb8af8a010[bar], $abc$import$558d6cfb8af8a010.baz);
    "#});
  }

  #[test]
  fn fold_require_member() {
    // let (_collect, code, _hoist) = parse(r#"
    // let foo;
    // ({foo} = require('other'));
    // console.log(foo);
    // "#);

    // println!("{}", code);

    let (_collect, code, _hoist) = parse(r#"
    const foo = require('other').foo;
    console.log(foo);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    console.log($abc$import$558d6cfb8af8a010$ba02ad2230917043);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    const foo = require('other')[bar];
    console.log(foo);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    const $abc$var$foo = $abc$import$558d6cfb8af8a010[bar];
    console.log($abc$var$foo);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    const {foo} = require('other').foo;
    console.log(foo);
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    const { foo: $abc$var$foo  } = $abc$import$558d6cfb8af8a010$ba02ad2230917043;
    console.log($abc$var$foo);
    "#});
  }

  #[test]
  fn fold_require_wrapped() {
    let (_collect, code, _hoist) = parse(r#"
    function x() {
      const foo = require('other');
      console.log(foo.bar);
    }
    require('bar');
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    function $abc$var$x() {
        const foo = $abc$import$558d6cfb8af8a010;
        console.log(foo.bar);
    }
    import   "abc:bar";
    $abc$import$3705fc5f2281438d;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    function x() {
      const foo = require('other').foo;
      console.log(foo);
    }
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    function $abc$var$x() {
        const foo = $abc$import$558d6cfb8af8a010$ba02ad2230917043;
        console.log(foo);
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    function x() {
      console.log(require('other').foo);
    }
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    function $abc$var$x() {
        console.log($abc$import$558d6cfb8af8a010$ba02ad2230917043);
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    function x() {
      const foo = require('other')[test];
      console.log(foo);
    }
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    function $abc$var$x() {
        const foo = $abc$import$558d6cfb8af8a010[test];
        console.log(foo);
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    function x() {
      const {foo} = require('other');
      console.log(foo);
    }
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    function $abc$var$x() {
        const { foo: foo  } = $abc$import$558d6cfb8af8a010;
        console.log(foo);
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    let x = require('a') + require('b');
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:a";
    import   "abc:b";
    let $abc$var$x = $abc$import$7ac649919d1c80a6 + $abc$import$a91ade93898ecc38;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    let x = (require('a'), require('b'));
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:a";
    import   "abc:b";
    let $abc$var$x = (!$abc$import$7ac649919d1c80a6, $abc$import$a91ade93898ecc38);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    let x = require('a') || require('b');
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:a";
    import   "abc:b";
    let $abc$var$x = $abc$import$7ac649919d1c80a6 || $abc$import$a91ade93898ecc38;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    let x = condition ? require('a') : require('b');
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:a";
    import   "abc:b";
    let $abc$var$x = condition ? $abc$import$7ac649919d1c80a6 : $abc$import$a91ade93898ecc38;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    if (condition) require('a');
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:a";
    if (condition) $abc$import$7ac649919d1c80a6;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    for (let x = require('y'); x < 5; x++) {}
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:y";
    for(let x = $abc$import$57bbd0ca114c72e; x < 5; x++){
    }
    "#});
  }

  #[test]
  fn fold_export() {
    let (_collect, code, _hoist) = parse(r#"
    let x = 3;
    let y = 4;
    let z = 6;
    export {x, y};
    "#);

    assert_eq!(code, indoc!{r#"
    let $abc$export$x = 3;
    let $abc$export$y = 4;
    let $abc$var$z = 6;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export default 3;
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$default = 3;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    let x = 3;
    export default x;
    "#);

    assert_eq!(code, indoc!{r#"
    let $abc$var$x = 3;
    var $abc$export$default = $abc$var$x;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export default function () {}
    "#);

    assert_eq!(code, indoc!{r#"
    function $abc$export$default() {
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export default class {}
    "#);

    assert_eq!(code, indoc!{r#"
    class $abc$export$default {
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export var x = 2, y = 3;
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$x = 2, $abc$export$y = 3;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export var {x, ...y} = something;
    export var [p, ...q] = something;
    export var {x = 3} = something;
    "#);

    assert_eq!(code, indoc!{r#"
    var { x: $abc$export$x , ...$abc$export$y } = something;
    var [$abc$export$p, ...$abc$export$q] = something;
    var { x: $abc$export$x = 3  } = something;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export function test() {}
    "#);

    assert_eq!(code, indoc!{r#"
    function $abc$export$test() {
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export class Test {}
    "#);

    assert_eq!(code, indoc!{r#"
    class $abc$export$Test {
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export {foo} from 'bar';
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:bar";
    "#});

    let (_collect, code, _hoist) = parse(r#"
    export * from 'bar';
    "#);

    assert_eq!(code, indoc!{r#"
    import   "abc:bar";
    "#});
  }

  #[test]
  fn fold_cjs_export() {
    let (_collect, code, _hoist) = parse(r#"
    exports.foo = 2;
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$foo;
    $abc$export$foo = 2;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    exports['foo'] = 2;
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$foo;
    $abc$export$foo = 2;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    function init() {
      exports.foo = 2;
    }
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$foo;
    function $abc$var$init() {
        $abc$export$foo = 2;
    }
    "#});

    let (_collect, code, _hoist) = parse(r#"
    module.exports.foo = 2;
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$foo;
    $abc$export$foo = 2;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    module.exports['foo'] = 2;
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$foo;
    $abc$export$foo = 2;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    exports.foo = 2;
    console.log(exports.foo)
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$foo;
    $abc$export$foo = 2;
    console.log($abc$export$foo);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    module.exports.foo = 2;
    console.log(module.exports.foo)
    "#);

    assert_eq!(code, indoc!{r#"
    var $abc$export$foo;
    $abc$export$foo = 2;
    console.log($abc$export$foo);
    "#});
  }

  #[test]
  fn fold_cjs_export_non_static() {
    let (_collect, code, _hoist) = parse(r#"
    exports[foo] = 2;
    exports.bar = 3;
    "#);

    assert_eq!(code, indoc!{r#"
    $abc$exports[foo] = 2;
    $abc$exports.bar = 3;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    module.exports[foo] = 2;
    module.exports.bar = 3;
    "#);

    assert_eq!(code, indoc!{r#"
    $abc$exports[foo] = 2;
    $abc$exports.bar = 3;
    "#});

    let (_collect, code, _hoist) = parse(r#"
    exports.foo = 2;
    sideEffects(exports);
    "#);

    assert_eq!(code, indoc!{r#"
    $abc$exports.foo = 2;
    sideEffects($abc$exports);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    exports.foo = 2;
    sideEffects(module.exports);
    "#);

    assert_eq!(code, indoc!{r#"
    $abc$exports.foo = 2;
    sideEffects($abc$exports);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    exports[foo] = 2;
    console.log(exports[foo]);
    "#);

    assert_eq!(code, indoc!{r#"
    $abc$exports[foo] = 2;
    console.log($abc$exports[foo]);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    exports[foo] = 2;
    console.log(exports.foo);
    "#);

    assert_eq!(code, indoc!{r#"
    $abc$exports[foo] = 2;
    console.log($abc$exports.foo);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    module.exports[foo] = 2;
    console.log(module.exports[foo]);
    "#);

    assert_eq!(code, indoc!{r#"
    $abc$exports[foo] = 2;
    console.log($abc$exports[foo]);
    "#});

    let (_collect, code, _hoist) = parse(r#"
    module.exports[foo] = 2;
    console.log(module.exports.foo);
    "#);

    assert_eq!(code, indoc!{r#"
    $abc$exports[foo] = 2;
    console.log($abc$exports.foo);
    "#});
  }

  #[test]
  fn fold_dynamic_import() {
    let (_collect, code, hoist) = parse(r#"
    async function test() {
      const x = await import('other');
      console.log(x.foo);
    }
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    async function $abc$var$test() {
        const x = await $abc$importAsync$558d6cfb8af8a010;
        console.log(x.foo);
    }
    "#});

    let (_collect, code, hoist) = parse(r#"
    async function test() {
      const x = await import('other');
      console.log(x[foo]);
    }
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => (w!("other"), w!("*"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    async function $abc$var$test() {
        const x = await $abc$importAsync$558d6cfb8af8a010;
        console.log(x[foo]);
    }
    "#});

    let (_collect, code, hoist) = parse(r#"
    async function test() {
      const {foo} = await import('other');
      console.log(foo);
    }
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    async function $abc$var$test() {
        const { foo: foo  } = await $abc$importAsync$558d6cfb8af8a010;
        console.log(foo);
    }
    "#});

    let (_collect, code, hoist) = parse(r#"
    async function test() {
      const {foo: bar} = await import('other');
      console.log(bar);
    }
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    async function $abc$var$test() {
        const { foo: bar  } = await $abc$importAsync$558d6cfb8af8a010;
        console.log(bar);
    }
    "#});

    let (_collect, code, hoist) = parse(r#"
    import('other').then(x => x.foo);
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    $abc$importAsync$558d6cfb8af8a010.then((x)=>x.foo
    );
    "#});

    let (_collect, code, hoist) = parse(r#"
    import('other').then(x => x);
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => (w!("other"), w!("*"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    $abc$importAsync$558d6cfb8af8a010.then((x)=>x
    );
    "#});

    let (_collect, code, hoist) = parse(r#"
    import('other').then(({foo}) => foo);
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    $abc$importAsync$558d6cfb8af8a010.then(({ foo: foo  })=>foo
    );
    "#});

    let (_collect, code, hoist) = parse(r#"
    import('other').then(({foo: bar}) => bar);
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    $abc$importAsync$558d6cfb8af8a010.then(({ foo: bar  })=>bar
    );
    "#});

    let (_collect, code, hoist) = parse(r#"
    import('other').then(function (x) { return x.foo });
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    $abc$importAsync$558d6cfb8af8a010.then(function(x) {
        return x.foo;
    });
    "#});

    let (_collect, code, hoist) = parse(r#"
    import('other').then(function (x) { return x });
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => (w!("other"), w!("*"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    $abc$importAsync$558d6cfb8af8a010.then(function(x) {
        return x;
    });
    "#});

    let (_collect, code, hoist) = parse(r#"
    import('other').then(function ({foo}) {});
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    $abc$importAsync$558d6cfb8af8a010.then(function({ foo: foo  }) {
    });
    "#});

    let (_collect, code, hoist) = parse(r#"
    import('other').then(function ({foo: bar}) {});
    "#);
    assert_eq_imported_symbols!(hoist.imported_symbols, map!{
      w!("$abc$importAsync$558d6cfb8af8a010$ba02ad2230917043") => (w!("other"), w!("foo"))
    });
    assert_eq!(hoist.dynamic_imports, map!{
      w!("$abc$importAsync$558d6cfb8af8a010") => w!("other")
    });
    assert_eq!(code, indoc!{r#"
    import   "abc:other";
    $abc$importAsync$558d6cfb8af8a010.then(function({ foo: bar  }) {
    });
    "#});
  }
}

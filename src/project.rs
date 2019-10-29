use crate::error::Error;
use crate::typ::ModuleTypeInfo;
use petgraph::Graph;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use strum_macros::{Display, EnumString, EnumVariantNames};

#[derive(Debug, PartialEq)]
pub struct Input {
    pub source_base_path: PathBuf,
    pub path: PathBuf,
    pub src: String,
    pub origin: ModuleOrigin,
}

#[derive(Debug, PartialEq)]
pub struct Compiled {
    pub name: Vec<String>,
    pub origin: ModuleOrigin,
    pub files: Vec<OutputFile>,
    pub type_info: ModuleTypeInfo,
}

#[derive(Debug, PartialEq)]
pub struct OutputFile {
    pub contents: String,
    pub path: PathBuf,
}

#[derive(Debug, PartialEq, Clone)]
pub enum ModuleOrigin {
    Src,
    Test,
    Dependency,
}

impl ModuleOrigin {
    pub fn dir_name(&self) -> &'static str {
        match self {
            ModuleOrigin::Src | ModuleOrigin::Dependency => "src",
            ModuleOrigin::Test => "test",
        }
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Display, EnumString, EnumVariantNames)]
#[strum(serialize_all = "kebab_case")]
pub enum RenderDocs {
    True,
    False,
}

pub fn compile(srcs: Vec<Input>, render_docs: RenderDocs) -> Result<Vec<Compiled>, Error> {
    struct Module {
        src: String,
        path: PathBuf,
        source_base_path: PathBuf,
        origin: ModuleOrigin,
        module: crate::ast::UntypedModule,
    }
    let module_count = srcs.len();
    let mut deps_graph = Graph::new();
    let mut indexes = HashMap::new();
    let mut modules: HashMap<_, Module> = HashMap::new();

    for Input {
        source_base_path,
        path,
        src,
        origin,
    } in srcs
    {
        let name = path
            .strip_prefix(source_base_path.clone())
            .unwrap()
            .parent()
            .unwrap()
            .join(path.file_stem().unwrap())
            .to_str()
            .unwrap()
            .to_string();
        let mut module = crate::grammar::ModuleParser::new()
            .parse(&crate::parser::strip_extra(&src))
            .map_err(|e| Error::Parse {
                path: path.clone(),
                src: src.clone(),
                error: e.map_token(|crate::grammar::Token(a, b)| (a, b.to_string())),
            })?;

        if let Some(Module {
            path: first_path, ..
        }) = indexes.get(&name).and_then(|i| modules.get(i))
        {
            return Err(Error::DuplicateModule {
                module: name,
                first: first_path.clone(),
                second: path,
            });
        }

        module.name = name.split("/").map(|s| s.to_string()).collect();

        let index = deps_graph.add_node(name.clone());
        indexes.insert(name.clone(), index);
        modules.insert(
            index,
            Module {
                src,
                path,
                module,
                origin,
                source_base_path,
            },
        );
    }

    // Register each module's deps so that we can determine a correct order to compile the modules.
    for module in modules.values() {
        let module_name = module.module.name_string();
        let src = module.src.clone();
        let path = module.path.clone();
        let deps = module.module.dependencies();
        let module_index = indexes
            .get(&module_name)
            .expect("Unable to find module index");
        let module = modules
            .get(&module_index)
            .expect("Unable to find module for index");

        for (dep, meta) in deps {
            let dep_index = indexes.get(&dep).ok_or_else(|| Error::UnknownImport {
                module: module_name.clone(),
                import: dep.clone(),
                src: src.clone(),
                path: path.clone(),
                modules: modules.values().map(|m| m.module.name_string()).collect(),
                meta: meta.clone(),
            })?;

            if module.origin == ModuleOrigin::Src
                && modules
                    .get(&dep_index)
                    .expect("Unable to find module for dep index")
                    .origin
                    == ModuleOrigin::Test
            {
                return Err(Error::SrcImportingTest {
                    path: path.clone(),
                    src: src.clone(),
                    meta,
                    src_module: module_name,
                    test_module: dep,
                });
            }

            deps_graph.add_edge(dep_index.clone(), module_index.clone(), ());
        }
    }

    let mut modules_type_infos = HashMap::new();
    let mut compiled_modules = Vec::with_capacity(module_count);

    struct Out {
        name_string: String,
        name: Vec<String>,
        origin: ModuleOrigin,
        files: Vec<OutputFile>,
    }

    for i in petgraph::algo::toposort(&deps_graph, None)
        .map_err(|_| Error::DependencyCycle)?
        .into_iter()
    {
        let Module {
            src,
            path,
            module,
            origin,
            source_base_path,
        } = modules.remove(&i).expect("Unknown graph index");
        let name = module.name.clone();
        let name_string = module.name_string();

        println!("Compiling {}", name_string);

        // Type check module
        let module = crate::typ::infer_module(module, &modules_type_infos)
            .map_err(|error| Error::Type { path, src, error })?;

        let gen_dir = source_base_path.parent().unwrap().join("gen");
        let path = gen_dir
            .join(origin.dir_name())
            .join(format!("{}.erl", module.name.join("@")));

        // Record module type information for use in compilation later modules
        modules_type_infos.insert(name_string.clone(), module.type_info.clone());

        // Compile Erlang source
        let contents = crate::erl::module(module);

        let mut files = vec![OutputFile { path, contents }];

        // Render module documentation if required
        if origin == ModuleOrigin::Src && render_docs == RenderDocs::True {
            let mut path = name
                .iter()
                .fold(gen_dir.join("docs"), |init, segment| init.join(segment));
            path.set_extension("md");
            let contents = "hey look some docs".to_string(); // TODO
            files.push(OutputFile { path, contents });
        }

        // Store compiled output
        compiled_modules.push(Out {
            name,
            name_string,
            origin,
            files,
        });
    }

    Ok(compiled_modules
        .into_iter()
        .map(|out| Compiled {
            name: out.name,
            files: out.files,
            origin: out.origin,
            type_info: modules_type_infos
                .remove(&out.name_string)
                .expect("merging module type info"),
        })
        .collect())
}

pub fn collect_source(src_dir: PathBuf, origin: ModuleOrigin, srcs: &mut Vec<Input>) {
    let src_dir = match src_dir.canonicalize() {
        Ok(d) => d,
        Err(_) => return,
    };
    let is_gleam_path = |e: &walkdir::DirEntry| {
        use regex::Regex;
        lazy_static! {
            static ref RE: Regex =
                Regex::new("^([a-z_]+/)*[a-z_]+\\.gleam$").expect("collect_source RE regex");
        }

        RE.is_match(
            e.path()
                .strip_prefix(&*src_dir)
                .expect("collect_source strip_prefix")
                .to_str()
                .unwrap_or(""),
        )
    };

    walkdir::WalkDir::new(src_dir.clone())
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(is_gleam_path)
        .for_each(|dir_entry| {
            let src = std::fs::read_to_string(dir_entry.path())
                .unwrap_or_else(|_| panic!("Unable to read {:?}", dir_entry.path()));

            srcs.push(Input {
                path: dir_entry
                    .path()
                    .canonicalize()
                    .expect("collect_source path canonicalize"),
                source_base_path: src_dir.clone(),
                origin: origin.clone(),
                src,
            })
        });
}

#[test]
fn compile_test() {
    struct Case {
        input: Vec<Input>,
        expected: Result<Vec<Output>, Error>,
    }
    #[derive(Debug, PartialEq)]
    struct Output {
        name: Vec<String>,
        files: Vec<OutputFile>,
        origin: ModuleOrigin,
    }

    let cases = vec![
        Case {
            input: vec![],
            expected: Ok(vec![]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    source_base_path: PathBuf::from("/src"),
                    path: PathBuf::from("/src/one.gleam"),
                    src: "".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    source_base_path: PathBuf::from("/src"),
                    path: PathBuf::from("/src/two.gleam"),
                    src: "".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![Input {
                origin: ModuleOrigin::Test,
                source_base_path: PathBuf::from("/test"),
                path: PathBuf::from("/test/one.gleam"),
                src: "".to_string(),
            }],
            expected: Ok(vec![Output {
                origin: ModuleOrigin::Test,
                name: vec!["one".to_string()],
                files: vec![OutputFile {
                    path: PathBuf::from("/gen/test/one.erl"),
                    contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                }],
            }]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Test,
                    source_base_path: PathBuf::from("/test"),
                    path: PathBuf::from("/test/two.gleam"),
                    src: "".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    source_base_path: PathBuf::from("/src"),
                    path: PathBuf::from("/src/one.gleam"),
                    src: "import two".to_string(),
                },
            ],
            expected: Err(Error::SrcImportingTest {
                path: PathBuf::from("/src/one.gleam"),
                src: "import two".to_string(),
                meta: crate::ast::Meta { start: 7, end: 10 },
                src_module: "one".to_string(),
                test_module: "two".to_string(),
            }),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import two".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import one".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub enum Box { Box(Int) }".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import one pub fn unbox(x) { let one.Box(i) = x i }".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents:
                            "-module(two).\n-compile(no_auto_import).\n\n-export([unbox/1]).\n
unbox(X) ->\n    {box, I} = X,\n    I.\n"
                                .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Dependency,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub enum Box { Box(Int) }".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Dependency,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import one pub fn box(x) { one.Box(x) }".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Dependency,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Dependency,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n\n-export([box/1]).\n
box(X) ->\n    {box, X}.\n"
                            .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![Input {
                origin: ModuleOrigin::Src,
                path: PathBuf::from("/src/one/two.gleam"),
                source_base_path: PathBuf::from("/src"),
                src: "pub enum Box { Box }".to_string(),
            }],
            expected: Ok(vec![Output {
                origin: ModuleOrigin::Src,
                name: vec!["one".to_string(), "two".to_string()],
                files: vec![OutputFile {
                    path: PathBuf::from("/gen/src/one@two.erl"),
                    contents: "-module(one@two).\n-compile(no_auto_import).\n\n\n".to_string(),
                }],
            }]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub enum Box { Box }".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import one pub fn box() { one.Box }".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n\n-export([box/0]).\n
box() ->\n    box.\n"
                            .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub fn go() { 1 }".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import one as thingy       pub fn call() { thingy.go() }".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n-export([go/0]).\n
go() ->
    1.\n"
                            .to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n\n-export([call/0]).\n
call() ->
    one:go().\n"
                            .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/nested/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub enum Box { Box(Int) }".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import nested/one\npub fn go(x) { let one.Box(y) = x y }".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["nested".to_string(), "one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/nested@one.erl"),
                        contents: "-module(nested@one).\n-compile(no_auto_import).\n\n\n"
                            .to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n\n-export([go/1]).
\ngo(X) ->\n    {box, Y} = X,\n    Y.\n"
                            .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/nested/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub enum Box { Box(Int) }".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import nested/one as thingy\npub fn go(x) { let thingy.Box(y) = x y }"
                        .to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["nested".to_string(), "one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/nested@one.erl"),
                        contents: "-module(nested@one).\n-compile(no_auto_import).\n\n\n"
                            .to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n\n-export([go/1]).
\ngo(X) ->\n    {box, Y} = X,\n    Y.\n"
                            .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/nested/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub external type Thing pub fn go() { 1 }".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import nested/one
                        pub fn go() { one.go() }
                        pub external fn thing() -> one.Thing = \"thing\" \"new\""
                        .to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["nested".to_string(), "one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/nested@one.erl"),
                        contents:
                            "-module(nested@one).\n-compile(no_auto_import).\n\n-export([go/0]).\n
go() ->\n    1.\n"
                                .to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents:
                            "-module(two).\n-compile(no_auto_import).\n\n-export([go/0, thing/0]).\n
go() ->\n    nested@one:go().\n
thing() ->\n    thing:new().\n"
                                .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/other/src/one.gleam"),
                    source_base_path: PathBuf::from("/other/src"),
                    src: "".to_string(),
                },
            ],
            expected: Err(Error::DuplicateModule {
                module: "one".to_string(),
                first: PathBuf::from("/src/one.gleam"),
                second: PathBuf::from("/other/src/one.gleam"),
            }),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub struct Point { x: Int y: Int }".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import one
                        fn make() { one.Point(1, 4) }
                        fn x(p) { let one.Point(x, _) = p x }"
                        .to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n
make() ->\n    {1, 4}.\n
x(P) ->\n    {X, _} = P,\n    X.\n"
                            .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub struct Empty {}".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import one
                        fn make() { one.Empty }"
                        .to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n\n".to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents:
                            "-module(two).\n-compile(no_auto_import).\n\nmake() ->\n    {}.\n"
                                .to_string(),
                    }],
                },
            ]),
        },
        Case {
            input: vec![
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/one.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "pub fn id(x) { x } pub struct Empty {}".to_string(),
                },
                Input {
                    origin: ModuleOrigin::Src,
                    path: PathBuf::from("/src/two.gleam"),
                    source_base_path: PathBuf::from("/src"),
                    src: "import one.{Empty, id} fn make() { id(Empty) }".to_string(),
                },
            ],
            expected: Ok(vec![
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["one".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/one.erl"),
                        contents: "-module(one).\n-compile(no_auto_import).\n\n-export([id/1]).\n
id(X) ->\n    X.\n"
                            .to_string(),
                    }],
                },
                Output {
                    origin: ModuleOrigin::Src,
                    name: vec!["two".to_string()],
                    files: vec![OutputFile {
                        path: PathBuf::from("/gen/src/two.erl"),
                        contents: "-module(two).\n-compile(no_auto_import).\n
make() ->\n    one:id({}).\n"
                            .to_string(),
                    }],
                },
            ]),
        },
    ];

    for Case { input, expected } in cases.into_iter() {
        let output = compile(input, RenderDocs::False).map(|mods| {
            mods.into_iter()
                .map(|compiled| Output {
                    name: compiled.name,
                    files: compiled.files,
                    origin: compiled.origin,
                })
                .collect::<Vec<_>>()
        });
        assert_eq!(expected, output);
    }
}

#[test]
fn module_docs_generation_test() {
    // src modules get docs
    let input = vec![Input {
        origin: ModuleOrigin::Src,
        path: PathBuf::from("/src/one/two/three.gleam"),
        source_base_path: PathBuf::from("/src"),
        src: "pub fn id(x) { x }".to_string(),
    }];
    let output = compile(input, RenderDocs::True).expect("should compile");
    assert_eq!(2, output[0].files.len());
    assert_eq!(
        PathBuf::from("/gen/docs/one/two/three.md"),
        output[0].files[1].path
    );

    // test modules do not get docs
    let input = vec![Input {
        origin: ModuleOrigin::Test,
        path: PathBuf::from("/src/one/two/three.gleam"),
        source_base_path: PathBuf::from("/src"),
        src: "pub fn id(x) { x }".to_string(),
    }];
    let output = compile(input, RenderDocs::True).expect("should compile");
    assert_eq!(1, output[0].files.len());

    // dependency modules do not get docs
    let input = vec![Input {
        origin: ModuleOrigin::Dependency,
        path: PathBuf::from("/src/one/two/three.gleam"),
        source_base_path: PathBuf::from("/src"),
        src: "pub fn id(x) { x }".to_string(),
    }];
    let output = compile(input, RenderDocs::True).expect("should compile");
    assert_eq!(1, output[0].files.len());
}

//! TypeScript under djs: a plugin written in it needs no transpile step, and an
//! unbundled provider still needs its import graph. In process only: no sockets, no child
//! processes, no signals.

#![cfg(feature = "engine-djs")]

use std::fs;
use std::path::Path;

use djs_embed::{Data, Error, Limits, ModuleMap, Snapshot, Source};

/// The pinned provider the github-issues bundle is built from, next to the plugin directory.
const PROVIDER: &str = "../.vendor/sourcefed/providers/github/src/issues/poll.ts";

/// Annotations, an interface, a generic function and an `as` cast: every one of them is a
/// type, so djs erases it while parsing and runs what is left as JavaScript.
const TYPESCRIPT: &str = r#"
interface Hit { name: string; stars: number }
type Ranked = Hit & { rank: number }

function rank<T extends Hit>(hits: T[]): Ranked[] {
  return hits
    .slice()
    .sort((a: Hit, b: Hit) => b.stars - a.stars)
    .map((hit, index) => ({ ...hit, rank: index + 1 }) as Ranked)
}

export function answer(): string {
  const hits: Hit[] = [
    { name: "djs", stars: 120 },
    { name: "quickjs", stars: 9000 },
  ]
  const label = "top" as string
  const best: Ranked = rank(hits)[0]
  return label + ": " + best.name + " " + best.rank
}
"#;

#[test]
fn typescript_is_erased_while_parsing() {
    let snapshot = Snapshot::builder()
        .source(Source::typescript_module("typescript.ts", TYPESCRIPT))
        .build()
        .expect("the TypeScript module builds");
    let mut instance = snapshot
        .instantiate(Limits::default())
        .expect("instantiate");

    let answer = instance
        .call_export("typescript.ts", "answer", &[], |_call| Ok(()))
        .expect("answer() runs");
    assert_eq!(answer, Data::String("top: quickjs 1".to_string()));
}

/// Without a bundler the pinned provider does not load: djs erases its types, then asks the
/// loader for `valibot` and `@sourcefed/core`. The imports are why the harness still ships
/// one esbuild bundle per plugin, and djs replaces the transpile step, not the bundling step.
#[test]
fn an_unbundled_provider_still_needs_its_imports() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(PROVIDER);
    let Ok(source) = fs::read_to_string(&path) else {
        println!("skipped: {} is missing", path.display());
        return;
    };

    // A loader that knows nothing: the same position the stall would be in if it handed
    // djs the provider's source instead of the bundle.
    let built = Snapshot::builder()
        .loader(ModuleMap::new())
        .source(Source::typescript_module("poll.ts", &source))
        .build();

    match built {
        Err(Error::Load { module, message }) => {
            println!("Error::Load for {module}: {message}");
            assert_eq!(module, "poll.ts");
            assert!(
                message.contains("cannot find module 'valibot'"),
                "message: {message}"
            );
        }
        Err(other) => panic!("the provider's types should erase and its imports fail: {other}"),
        Ok(_) => panic!("the unbundled provider built a snapshot with no loader"),
    }
}

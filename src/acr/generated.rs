//! Where generated files are, by the names and folders code generators use.
//! This is the one place ACR knows any ecosystem's conventions, and only
//! names no hand-written file would have. A repository adds its own in
//! `generated.paths`, and keeps any of these in `generated.keep`.
//!
//! Never by a file's contents, nor by anything the change itself can set: a
//! change mustn't be able to have its own code left out of its review.

/// Paths that are generated wherever they appear.
pub const CONVENTIONS: &[&str] = &[
    // Build output, as Cargo, Maven and sbt write it.
    "**/target/**",
    // Dart's build_runner, freezed, mockito and auto_route.
    "**/*.g.dart",
    "**/*.freezed.dart",
    "**/*.mocks.dart",
    "**/*.gr.dart",
    // injectable names its output after the file it's set up in, so only
    // the names that setups commonly use, never `*.config.dart`.
    "**/injectable.config.dart",
    "**/injection.config.dart",
    // Protocol buffers and gRPC, in each language's output names.
    "**/*.pb.go",
    "**/*_grpc.pb.go",
    "**/*.pb.gw.go",
    "**/*_pb2.py",
    "**/*_pb2_grpc.py",
    "**/*_pb2.pyi",
    "**/*.pb.dart",
    "**/*.pbenum.dart",
    "**/*.pbjson.dart",
    "**/*.pbgrpc.dart",
    "**/*.pb.cc",
    "**/*.pb.h",
    "**/*_pb.js",
    "**/*_pb.d.ts",
    // Folders generators write to by name.
    "**/generated/**",
    "**/__generated__/**",
    // Minified bundles.
    "**/*.min.js",
    "**/*.min.css",
    // Lockfiles, which a dependencies lens can still take with
    // `include_generated`.
    "**/Cargo.lock",
    "**/package-lock.json",
    "**/yarn.lock",
    "**/pnpm-lock.yaml",
    "**/pubspec.lock",
    "**/go.sum",
    "**/poetry.lock",
    "**/uv.lock",
    "**/Gemfile.lock",
    "**/composer.lock",
    "**/Podfile.lock",
];

#[cfg(test)]
mod tests {
    use super::*;
    use globset::{Glob, GlobSetBuilder};

    #[test]
    fn conventions_name_generators_output_and_nothing_hand_written() {
        let mut set = GlobSetBuilder::new();
        for p in CONVENTIONS {
            set.add(Glob::new(p).unwrap());
        }
        let set = set.build().unwrap();
        for generated in [
            "lib/database/database.g.dart",
            "lib/configuration/injectable.config.dart",
            "lib/di/injection.config.dart",
            "crates/x/target/debug/out.rs",
            "Cargo.lock",
        ] {
            assert!(set.is_match(generated), "{generated}");
        }
        for written in [
            "lib/app.config.dart",
            "lib/main.dart",
            "src/target.rs",
            "docs/build.md",
        ] {
            assert!(!set.is_match(written), "{written}");
        }
    }
}

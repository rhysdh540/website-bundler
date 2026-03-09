pub mod dev_server;
pub mod templating;

use crate::templating::{Frontmatter, TemplateEngine, tokenize};
use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use minify_html::{Cfg, minify as minify_html};
use oxc::codegen::{Codegen as JsCodegen, CodegenOptions, CommentOptions};
use oxc::minifier::{CompressOptions, MangleOptions, Minifier as JsMinifier, MinifierOptions};
use oxc::parser::Parser as JsParser;
use oxc::span::SourceType;
use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, Parser, Default)]
#[command(author, version, about)]
pub struct BuildOptions {
    #[arg(long, short, value_name = "DIR")]
    pub in_dir: PathBuf,
    #[arg(long, short, value_name = "DIR")]
    pub out_dir: PathBuf,
    #[arg(long, value_name = "DIR")]
    pub include_dir: PathBuf,
}

fn is_dot_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.starts_with('.'))
}

fn normalize_web_path(s: &str) -> Result<PathBuf> {
    if !s.starts_with('/') {
        bail!("frontmatter.path must start with '/': got {s}");
    }
    let trimmed = s.trim_start_matches('/');
    Ok(PathBuf::from(trimmed))
}

fn default_output_path(rel_source_path: &Path) -> PathBuf {
    let file_name = rel_source_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or_default();

    if file_name.eq_ignore_ascii_case("index.html") {
        return rel_source_path.to_path_buf();
    }

    if rel_source_path.extension().and_then(|e| e.to_str()) == Some("html") {
        let stem = rel_source_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("page");
        let parent = rel_source_path.parent().unwrap_or(Path::new(""));
        return parent.join(stem).join("index.html");
    }

    rel_source_path.to_path_buf()
}

fn minify(html: String) -> Result<Vec<u8>> {
    let minified = minify_html(
        html.as_bytes(),
        &Cfg {
            minify_css: true,
            minify_js: false,
            keep_comments: true,
            ..Cfg::default()
        },
    );

    // scan for all scripts
    let mut scripts_to_minify = Vec::new();
    let mut i: usize = 0;

    while i < minified.len() {
        let Some(rel_start) = find_subslice(&minified[i..], b"<script") else {
            break;
        };
        let start = i + rel_start;
        let haystack = &minified[start..];
        let Some(open_end_rel) = haystack.iter().position(|&x| x == b'>') else {
            break;
        };
        let open_end = start + open_end_rel;
        let open_tag = &minified[start..=open_end];
        let open_tag_lc = open_tag.to_ascii_lowercase();
        let should_minify = !find_subslice(&open_tag_lc, b"src=").is_some()
            && !find_subslice(&open_tag_lc, b"application/ld+json").is_some();

        let content_start = open_end + 1;
        let Some(close_rel) = find_subslice(&minified[content_start..], b"</script>") else {
            break;
        };
        let close_start = content_start + close_rel;
        let close_end = close_start + b"</script>".len();

        if should_minify {
            scripts_to_minify.push((content_start, close_start));
        }

        i = close_end;
    }

    // merge them all and minify
    let minified_js = if !scripts_to_minify.is_empty() {
        let mut combined_js = String::new();
        for (idx, &(start, end)) in scripts_to_minify.iter().enumerate() {
            let script_bytes = &minified[start..end];
            let script_str = std::str::from_utf8(script_bytes)
                .map_err(|e| anyhow!("Script contents were not valid UTF-8: {e}"))?;
            combined_js.push_str(script_str);
            combined_js.push('\n');
            if !combined_js.ends_with(";\n") {
                combined_js.push_str(";\n");
            }
            if idx < scripts_to_minify.len() - 1 {
                combined_js.push_str(&merge_marker(idx));
            }
        }

        match minify_js(combined_js) {
            Ok(js) => Some(js),
            Err(e) => {
                eprintln!("Failed to minify JS: {}", e);
                None
            }
        }
    } else {
        None
    };

    let mut out: Vec<u8> = Vec::with_capacity(minified.len());
    let mut i: usize = 0;
    let mut scripts_seen = 0;

    while i < minified.len() {
        let Some(rel_start) = find_subslice(&minified[i..], b"<script") else {
            out.extend_from_slice(&minified[i..]);
            break;
        };
        let start = i + rel_start;

        out.extend_from_slice(&minified[i..start]);

        let haystack = &minified[start..];
        let Some(open_end_rel) = haystack.iter().position(|&x| x == b'>') else {
            // malformed
            out.extend_from_slice(&minified[start..]);
            break;
        };
        let open_end = start + open_end_rel; // index of '>'
        let open_tag = &minified[start..=open_end];

        let open_tag_lc = open_tag.to_ascii_lowercase();
        let should_minify = !find_subslice(&open_tag_lc, b"src=").is_some()
            && !find_subslice(&open_tag_lc, b"application/ld+json").is_some();

        out.extend_from_slice(open_tag);

        let content_start = open_end + 1;
        let Some(close_rel) = find_subslice(&minified[content_start..], b"</script>") else {
            // malformed
            out.extend_from_slice(&minified[content_start..]);
            break;
        };
        let close_start = content_start + close_rel;
        let close_end = close_start + b"</script>".len();

        if should_minify {
            if let Some(ref js) = minified_js {
                let current_script_index = scripts_seen;
                scripts_seen += 1;

                let start_pos = if current_script_index == 0 {
                    0
                } else {
                    let prev_marker = merge_marker(current_script_index - 1);
                    match js.find(&prev_marker) {
                        Some(pos) => pos + prev_marker.len(),
                        None => {
                            eprintln!("Failed to find marker {}", prev_marker);
                            0
                        }
                    }
                };

                let end_pos = if current_script_index == scripts_to_minify.len() - 1 {
                    js.len()
                } else {
                    let marker = merge_marker(current_script_index);
                    match js.find(&marker) {
                        Some(pos) => pos,
                        None => {
                            eprintln!("Failed to find marker {}", marker);
                            js.len()
                        }
                    }
                };

                let mut script_part = &js[start_pos..end_pos];
                script_part = script_part.trim_start_matches(';');
                script_part = script_part.trim_end_matches(';');

                out.extend_from_slice(script_part.as_bytes());
            } else {
                out.extend_from_slice(&minified[content_start..close_start]);
                scripts_seen += 1;
            }
        } else {
            out.extend_from_slice(&minified[content_start..close_start]);
        }

        out.extend_from_slice(&minified[close_start..close_end]);

        i = close_end;
    }

    out.shrink_to_fit();
    Ok(out)
}

fn merge_marker(idx: usize) -> String {
    format!("try{{window.__JS_MERGE_THINGY_{}__=1}}catch{{}}", idx)
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn minify_js(js: String) -> Result<String> {
    let allocator = oxc::allocator::Allocator::default();

    let parse = JsParser::new(&allocator, &js, SourceType::script()).parse();

    if !parse.errors.is_empty() {
        bail!("Failed to parse JS: {}", parse.errors[0].to_string());
    }

    let mut prog = parse.program;
    let min_out = JsMinifier::new(MinifierOptions {
        mangle: Some(MangleOptions {
            top_level: Some(true),
            ..Default::default()
        }),
        compress: Some(CompressOptions::smallest()),
    })
    .minify(&allocator, &mut prog);

    Ok(JsCodegen::new()
        .with_options(CodegenOptions {
            minify: true,
            comments: CommentOptions::disabled(),
            ..Default::default()
        })
        .with_scoping(min_out.scoping)
        .with_private_member_mappings(min_out.class_private_mappings)
        .build(&prog)
        .code)
}

pub fn build_site(opts: BuildOptions) -> Result<std::time::Duration> {
    let start_time = std::time::Instant::now();
    let in_dir = opts.in_dir;
    let out_dir = opts.out_dir;

    if !in_dir.exists() {
        bail!("Input directory does not exist: {}", in_dir.display());
    }

    if out_dir.exists() {
        fs::remove_dir_all(&out_dir)
            .with_context(|| format!("Failed to remove out_dir {}", out_dir.display()))?;
    }
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("Failed to create out_dir {}", out_dir.display()))?;

    let engine = TemplateEngine::new(opts.include_dir.clone());

    for entry in WalkDir::new(&in_dir).follow_links(false) {
        let entry = entry?;
        let p = entry.path();

        let rel = p.strip_prefix(&in_dir)?.to_path_buf();

        // skip dotfiles
        if rel.components().any(|c| match c {
            std::path::Component::Normal(s) => s.to_str().is_some_and(|x| x.starts_with('.')),
            _ => false,
        }) {
            continue;
        }

        if entry.file_type().is_dir() {
            continue;
        }

        if rel.as_os_str().is_empty() {
            continue;
        }

        if is_dot_file(p) {
            continue;
        }

        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");

        if ext == "html" {
            let raw = fs::read_to_string(p)
                .with_context(|| format!("Failed to read html: {}", p.display()))?;
            let (frontmatter, body) = Frontmatter::try_parse(&raw)?;

            let mut output_rel = default_output_path(&*rel);

            let mut vars = frontmatter
                .as_ref()
                .map(|fm| fm.vars.clone())
                .unwrap_or_default();
            if let Some(path) = frontmatter.as_ref().and_then(|fm| fm.path.as_ref()) {
                output_rel = normalize_web_path(path.as_str())?;
                if output_rel.as_os_str().is_empty() {
                    bail!("frontmatter.path cannot be '/'");
                }
            }

            let (text_to_render, content, current_dir) =
                if let Some(tpl) = frontmatter.as_ref().and_then(|fm| fm.template.as_ref()) {
                    let template_path = opts.include_dir.join(tpl);
                    let tpl_txt = fs::read_to_string(&template_path).with_context(|| {
                        format!("Failed to read template: {}", template_path.display())
                    })?;
                    (tpl_txt, Some(body), opts.include_dir.as_path())
                } else {
                    (body, None, p.parent().unwrap())
                };

            let tokens = tokenize(&text_to_render)?;
            let page_dir = p.parent().unwrap();
            let content_tokens = content.map(|c| tokenize(&c).unwrap().into());
            let rendered =
                engine.render(&tokens, &mut vars, current_dir, page_dir, content_tokens)?;

            let out_path = out_dir.join(output_rel);
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }

            if fs::exists(&out_path)? {
                bail!("Output file already exists: {}", out_path.display());
            }

            fs::write(&out_path, minify(rendered)?)
                .with_context(|| format!("Failed to write {}", out_path.display()))?;
        } else {
            let out_path = out_dir.join(rel);
            if let Some(parent) = out_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(p, &out_path).with_context(|| {
                format!("Failed to copy {} -> {}", p.display(), out_path.display())
            })?;
        }
    }

    Ok(start_time.elapsed())
}

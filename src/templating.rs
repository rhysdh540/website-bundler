use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Map as JsonMap, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// frontmatter: <!--{...}--> at start of HTML file
/// contains JSON object with optional fields:
/// - template: string; name of template to use
/// - vars: object; variables to set for this page (can be nested)
/// - path: string; output path for this page (overrides default)
///
/// templating syntax: <!--#<function> p1="v1" p2="v2" ... -->
/// supported functions:
/// - include file="..."; includes relative to include_dir
/// - include rel="..."; includes relative to current dir
/// - content; if template, includes body, else error
/// - if expr="...", elif expr="...", else, endif; self-explanatory conditionals
/// - echo var="...", unset var="...", set var="..." val="..."; get/un/set variables
/// - env.* is read-only in expressions (set/unset rejects writes to that namespace)
#[derive(Debug, Clone)]
pub struct Frontmatter {
    pub template: Option<String>,
    pub vars: HashMap<String, String>,
    pub path: Option<String>,
}

impl Frontmatter {
    pub fn try_parse(html: &str) -> Result<(Option<Frontmatter>, String)> {
        let trimmed = html.trim_start();
        if !trimmed.starts_with("<!--") {
            return Ok((None, html.to_string()));
        }

        let Some(end_idx) = trimmed.find("-->") else {
            return Ok((None, html.to_string()));
        };

        let comment_body = &trimmed[4..end_idx];
        let comment_body = comment_body.trim();

        if !comment_body.starts_with('{') {
            return Ok((None, html.to_string()));
        }

        let parsed: Value = match json5::from_str(comment_body) {
            Ok(v) => v,
            Err(_) => bail!("Invalid JSON in frontmatter"),
        };

        let rest = &trimmed[end_idx + 3..];

        let template = match parsed.get("template") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => bail!("frontmatter.template must be a string"),
        };

        let vars = match parsed.get("vars") {
            None => JsonMap::new(),
            Some(Value::Object(o)) => o.clone(),
            Some(_) => bail!("frontmatter.vars must be an object"),
        };
        let vars = parse_frontmatter_vars(&vars)?;

        let path = match parsed.get("path") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => bail!("frontmatter.path must be a string"),
        };

        Ok((
            Some(Frontmatter {
                template,
                vars,
                path,
            }),
            rest.trim_start().to_string(),
        ))
    }
}

pub struct TemplateEngine {
    pub include_dir: PathBuf,
    pub token_cache: Mutex<HashMap<PathBuf, Arc<[Token]>>>,
}

impl TemplateEngine {
    pub fn new(include_dir: PathBuf) -> Self {
        Self {
            include_dir,
            token_cache: Mutex::new(HashMap::new()),
        }
    }
}

#[derive(Debug, Clone)]
pub enum Token {
    Text(String),
    Tag {
        name: String,
        params: Vec<(String, String)>,
    },
}

pub fn tokenize(text: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut pos = 0;

    // repeatedly find every tag
    while let Some(tag_start_rel) = text[pos..].find("<!--#") {
        let tag_start = pos + tag_start_rel;
        if tag_start > pos {
            tokens.push(Token::Text(text[pos..tag_start].to_string()));
        }

        let rest = &text[tag_start..];
        let Some(tag_end_rel) = rest.find("-->") else {
            bail!("Unclosed templating tag at position {}", tag_start);
        };
        let tag_end = tag_start + tag_end_rel;
        let tag_content = text[tag_start + 5..tag_end].trim();

        let (name, params) = parse_tag(tag_content)?;
        tokens.push(Token::Tag { name, params });

        pos = tag_end + 3;
    }

    // any remaining text
    if pos < text.len() {
        tokens.push(Token::Text(text[pos..].to_string()));
    }
    Ok(tokens)
}

fn parse_tag(s: &str) -> Result<(String, Vec<(String, String)>)> {
    let mut chars = s.chars().peekable();

    // name
    let mut name = String::new();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            break;
        }
        name.push(chars.next().unwrap());
    }

    if name.is_empty() {
        bail!("Empty tag name");
    }

    let mut params = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }

        let mut key = String::new();
        key.push(c);
        while let Some(&nc) = chars.peek() {
            if nc == '=' || nc.is_whitespace() {
                break;
            }
            key.push(chars.next().unwrap());
        }

        while let Some(&nc) = chars.peek() {
            if nc.is_whitespace() {
                chars.next();
            } else {
                break;
            }
        }

        if chars.peek() == Some(&'=') {
            chars.next();
            while let Some(&nc) = chars.peek() {
                if nc.is_whitespace() {
                    chars.next();
                } else {
                    break;
                }
            }
            if chars.peek() == Some(&'"') {
                chars.next();
                let mut val = String::new();
                while let Some(nc) = chars.next() {
                    if nc == '"' {
                        break;
                    }
                    val.push(nc);
                }
                params.push((key, val));
            }
        }
    }

    Ok((name, params))
}

fn resolve_vars(s: &str, vars: &HashMap<String, String>) -> String {
    if !s.contains("${") {
        return s.to_string();
    }
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '$' && chars.peek() == Some(&'{') {
            chars.next(); // skip {
            let mut var_name = String::new();
            let mut found_end = false;
            while let Some(nc) = chars.next() {
                if nc == '}' {
                    found_end = true;
                    break;
                }
                var_name.push(nc);
            }
            if found_end {
                if let Some(val) = vars.get(&var_name) {
                    result.push_str(val);
                }
            } else {
                result.push_str("${");
                result.push_str(&var_name);
            }
        } else {
            result.push(c);
        }
    }
    result
}

fn parse_frontmatter_vars(vars: &JsonMap<String, Value>) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for (k, v) in vars {
        if k.is_empty() {
            bail!("frontmatter.vars keys cannot be empty");
        }
        flatten_frontmatter_var(k, v, &mut out)?;
    }
    Ok(out)
}

fn flatten_frontmatter_var(
    path: &str,
    value: &Value,
    out: &mut HashMap<String, String>,
) -> Result<()> {
    match value {
        Value::String(s) => {
            ensure_legal(path)?;
            out.insert(path.to_string(), s.clone());
            Ok(())
        }
        Value::Object(map) => {
            for (k, v) in map {
                if k.is_empty() {
                    bail!("frontmatter.vars keys cannot be empty");
                }
                let nested_path = format!("{path}.{k}");
                flatten_frontmatter_var(&nested_path, v, out)?;
            }
            Ok(())
        }
        _ => bail!("frontmatter.vars values must be strings or objects"),
    }
}

fn ensure_legal(var_name: &str) -> Result<()> {
    if var_name.starts_with("env.") || var_name == "env" {
        bail!("Cannot write to env variables: '{}'", var_name);
    }
    if var_name == "true" || var_name == "false" {
        bail!("Cannot use reserved variable name: '{}'", var_name);
    }
    Ok(())
}

fn get_var(name: &str, vars: &HashMap<String, String>) -> Option<String> {
    if let Some(env_name) = name.strip_prefix("env.") {
        return std::env::var(env_name).ok();
    }
    vars.get(name).cloned()
}

fn parse_str_sq(s: &str) -> Result<String> {
    let bytes = s.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'\'' || bytes[bytes.len() - 1] != b'\'' {
        bail!("String comparisons must use single-quoted strings");
    }

    let inner = &s[1..s.len() - 1];
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    let mut escaped = false;
    while let Some(c) = chars.next() {
        if escaped {
            match c {
                '\'' | '\\' => out.push(c),
                'n' => out.push('\n'),
                't' => out.push('\t'),
                _ => out.push(c),
            }
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        out.push(c);
    }
    if escaped {
        bail!("Invalid trailing escape in string literal");
    }

    Ok(out)
}

fn remove_parens(expr: &str) -> Option<&str> {
    if !(expr.starts_with('(') && expr.ends_with(')')) {
        return None;
    }

    let mut depth = 0i32;
    let mut in_single_quote = false;
    let mut escaped = false;

    for (i, c) in expr.char_indices() {
        if in_single_quote {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '\'' => in_single_quote = false,
                _ => {}
            }
            continue;
        }

        match c {
            '\'' => in_single_quote = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != expr.len() - 1 {
                    return None;
                }
            }
            _ => {}
        }
    }

    if depth == 0 {
        Some(expr[1..expr.len() - 1].trim())
    } else {
        None
    }
}

fn evaluate_expr(expr: &str, vars: &HashMap<String, String>) -> Result<bool> {
    let expr = expr.trim();
    if expr.is_empty() {
        return Ok(false);
    }

    if let Some(idx) = find_operator(expr, "||") {
        return Ok(evaluate_expr(&expr[..idx], vars)? || evaluate_expr(&expr[idx + 2..], vars)?);
    }

    if let Some(idx) = find_operator(expr, "^") {
        return Ok(evaluate_expr(&expr[..idx], vars)? ^ evaluate_expr(&expr[idx + 1..], vars)?);
    }

    if let Some(idx) = find_operator(expr, "&&") {
        return Ok(evaluate_expr(&expr[..idx], vars)? && evaluate_expr(&expr[idx + 2..], vars)?);
    }

    if expr.starts_with('!') {
        return Ok(!evaluate_expr(expr[1..].trim_start(), vars)?);
    }

    if let Some(inner) = remove_parens(expr) {
        return evaluate_expr(inner, vars);
    }

    if let Some(idx) = find_operator(expr, "==") {
        let left = expr[..idx].trim();
        let right = expr[idx + 2..].trim();
        if left.is_empty() || right.is_empty() {
            bail!("Invalid comparison expression: '{expr}'");
        }
        let expected = parse_str_sq(right)?;
        return Ok(get_var(left, vars).is_some_and(|actual| actual == expected));
    }

    if expr == "true" {
        return Ok(true);
    }
    if expr == "false" {
        return Ok(false);
    }

    Ok(get_var(expr, vars).is_some())
}

fn find_operator(expr: &str, op: &str) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_single_quote = false;
    let mut escaped = false;

    for (i, c) in expr.char_indices() {
        if in_single_quote {
            if escaped {
                escaped = false;
                continue;
            }
            match c {
                '\\' => escaped = true,
                '\'' => in_single_quote = false,
                _ => {}
            }
            continue;
        }

        match c {
            '\'' => in_single_quote = true,
            '(' => depth += 1,
            ')' => depth -= 1,
            _ if depth == 0 && expr[i..].starts_with(op) => return Some(i),
            _ => {}
        }
    }
    None
}

impl TemplateEngine {
    pub fn render(
        &self,
        tokens: &[Token],
        vars: &mut HashMap<String, String>,
        current_dir: &Path,
        page_dir: &Path,
        content_tokens: Option<Arc<[Token]>>,
    ) -> Result<String> {
        let mut output = String::new();
        self.render_to_buf(
            tokens,
            vars,
            current_dir,
            page_dir,
            content_tokens,
            &mut output,
        )?;
        Ok(output)
    }

    pub fn render_to_buf(
        &self,
        tokens: &[Token],
        vars: &mut HashMap<String, String>,
        current_dir: &Path,
        page_dir: &Path,
        content_tokens: Option<Arc<[Token]>>,
        output: &mut String,
    ) -> Result<()> {
        let mut i = 0;
        while i < tokens.len() {
            match &tokens[i] {
                Token::Text(t) => {
                    output.push_str(t);
                    i += 1;
                }
                Token::Tag { name, params } => {
                    match name.as_str() {
                        "echo" => self.handle_echo(params, vars, output)?,
                        "set" => self.handle_set(params, vars)?,
                        "unset" => self.handle_unset(params, vars)?,
                        "include" => self.handle_include(
                            params,
                            vars,
                            current_dir,
                            page_dir,
                            content_tokens.clone(),
                            output,
                        )?,
                        "content" => {
                            self.handle_content(vars, page_dir, content_tokens.clone(), output)?
                        }
                        "if" => {
                            let new_i = self.handle_conditional_block(
                                tokens,
                                i,
                                vars,
                                current_dir,
                                page_dir,
                                content_tokens.clone(),
                                output,
                            )?;
                            i = new_i;
                            continue;
                        }
                        "elif" | "else" | "endif" => bail!("Unexpected tag: {}", name),
                        _ => bail!("Unknown function: {}", name),
                    }
                    i += 1;
                }
            }
        }
        Ok(())
    }

    fn handle_echo(
        &self,
        params: &[(String, String)],
        vars: &HashMap<String, String>,
        output: &mut String,
    ) -> Result<()> {
        let var_name =
            get_param(params, "var").ok_or_else(|| anyhow!("echo missing 'var' parameter"))?;
        let val = get_var(var_name, vars).unwrap_or_default();
        output.push_str(&val);
        Ok(())
    }

    fn handle_set(
        &self,
        params: &[(String, String)],
        vars: &mut HashMap<String, String>,
    ) -> Result<()> {
        let var_name =
            get_param(params, "var").ok_or_else(|| anyhow!("set missing 'var' parameter"))?;
        ensure_legal(var_name)?;
        let val = get_param(params, "val").ok_or_else(|| anyhow!("set missing 'val' parameter"))?;
        let resolved = resolve_vars(val, vars);
        vars.insert(var_name.to_string(), resolved);
        Ok(())
    }

    fn handle_unset(
        &self,
        params: &[(String, String)],
        vars: &mut HashMap<String, String>,
    ) -> Result<()> {
        let var_name =
            get_param(params, "var").ok_or_else(|| anyhow!("unset missing 'var' parameter"))?;
        ensure_legal(var_name)?;
        vars.remove(var_name);
        Ok(())
    }

    fn handle_include(
        &self,
        params: &[(String, String)],
        vars: &mut HashMap<String, String>,
        current_dir: &Path,
        page_dir: &Path,
        content_tokens: Option<Arc<[Token]>>,
        output: &mut String,
    ) -> Result<()> {
        let include_path = if let Some(file) = get_param(params, "file") {
            self.include_dir.join(file)
        } else if let Some(rel) = get_param(params, "rel") {
            current_dir.join(rel)
        } else {
            bail!("include missing 'file' or 'rel' parameter");
        };

        let tokens = {
            let mut cache = self.token_cache.lock().expect("Mutex poisoned");
            if let Some(tokens) = cache.get(&include_path) {
                tokens.clone()
            } else {
                let included_text = std::fs::read_to_string(&include_path).with_context(|| {
                    format!("Failed to read include file: {}", include_path.display())
                })?;
                let tokens: Arc<[Token]> = tokenize(&included_text)?.into();
                cache.insert(include_path.clone(), tokens.clone());
                tokens
            }
        };

        self.render_to_buf(
            &tokens,
            vars,
            include_path.parent().unwrap_or(current_dir),
            page_dir,
            content_tokens,
            output,
        )?;
        Ok(())
    }

    fn handle_content(
        &self,
        vars: &mut HashMap<String, String>,
        page_dir: &Path,
        content_tokens: Option<Arc<[Token]>>,
        output: &mut String,
    ) -> Result<()> {
        if let Some(tokens) = content_tokens {
            self.render_to_buf(&tokens, vars, page_dir, page_dir, None, output)?;
            Ok(())
        } else {
            bail!("content tag used outside of template");
        }
    }

    fn handle_conditional_block(
        &self,
        tokens: &[Token],
        start_idx: usize,
        vars: &mut HashMap<String, String>,
        current_dir: &Path,
        page_dir: &Path,
        content_tokens: Option<Arc<[Token]>>,
        output: &mut String,
    ) -> Result<usize> {
        let mut blocks: Vec<(Option<String>, usize, usize)> = Vec::new(); // (Option<expr>, start_idx, end_idx)

        let first_tag = &tokens[start_idx];
        let (mut current_expr, mut current_start) = match first_tag {
            Token::Tag { name, params } if name == "if" => {
                (get_param(params, "expr").map(str::to_owned), start_idx + 1)
            }
            _ => bail!("handle_conditional_block must start with 'if' tag"),
        };

        let mut depth = 0;
        let mut j = start_idx + 1;
        let mut found_endif = false;
        let mut next_i = 0;

        while j < tokens.len() {
            match &tokens[j] {
                Token::Tag { name, .. } if name == "if" => depth += 1,
                Token::Tag { name, .. } if name == "endif" => {
                    if depth == 0 {
                        blocks.push((current_expr, current_start, j));
                        next_i = j + 1;
                        found_endif = true;
                        break;
                    }
                    depth -= 1;
                }
                Token::Tag {
                    name,
                    params: next_params,
                } if (name == "elif" || name == "else") && depth == 0 => {
                    blocks.push((current_expr, current_start, j));
                    if name == "else" {
                        current_expr = Some("true".to_string());
                    } else {
                        current_expr = get_param(next_params, "expr").map(str::to_owned);
                    }
                    current_start = j + 1;
                }
                _ => {}
            }
            j += 1;
        }

        if !found_endif {
            bail!("Missing endif for 'if' tag");
        }

        for (expr, start, end) in blocks {
            let should_render = match expr {
                Some(e) => evaluate_expr(&e, vars)?,
                None => true,
            };
            if should_render {
                self.render_to_buf(
                    &tokens[start..end],
                    vars,
                    current_dir,
                    page_dir,
                    content_tokens,
                    output,
                )?;
                return Ok(next_i);
            }
        }

        Ok(next_i)
    }
}

fn get_param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

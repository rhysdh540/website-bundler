use std::collections::HashMap;
use std::path::{Path, PathBuf};
use anyhow::{Result, bail, Context, anyhow};
use serde_json::{Value, Map as JsonMap};
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
        }.iter().map(|(k, v)| {
            match v {
                Value::String(s) => Ok((k.clone(), s.clone())),
                _ => bail!("frontmatter.vars values must be strings"),
            }
        }).collect::<Result<HashMap<_, _>>>()?;

        let path = match parsed.get("path") {
            None => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => bail!("frontmatter.path must be a string"),
        };

        Ok((Some(Frontmatter { template, vars, path }), rest.trim_start().to_string()))
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
        if c.is_whitespace() { break; }
        name.push(chars.next().unwrap());
    }

    if name.is_empty() {
        bail!("Empty tag name");
    }

    let mut params = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() { continue; }

        let mut key = String::new();
        key.push(c);
        while let Some(&nc) = chars.peek() {
            if nc == '=' || nc.is_whitespace() { break; }
            key.push(chars.next().unwrap());
        }

        while let Some(&nc) = chars.peek() {
            if nc.is_whitespace() { chars.next(); } else { break; }
        }

        if chars.peek() == Some(&'=') {
            chars.next();
            while let Some(&nc) = chars.peek() {
                if nc.is_whitespace() { chars.next(); } else { break; }
            }
            if chars.peek() == Some(&'"') {
                chars.next();
                let mut val = String::new();
                while let Some(nc) = chars.next() {
                    if nc == '"' { break; }
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

fn evaluate_expression(expr: &str) -> bool {
    let expr = expr.trim();
    if expr.is_empty() {
        return false;
    }

    if let Some(idx) = find_operator(expr, "||") {
        return evaluate_expression(&expr[..idx]) || evaluate_expression(&expr[idx + 2..]);
    }

    if let Some(idx) = find_operator(expr, "^") {
        return evaluate_expression(&expr[..idx]) ^ evaluate_expression(&expr[idx + 1..]);
    }

    if let Some(idx) = find_operator(expr, "&&") {
        return evaluate_expression(&expr[..idx]) && evaluate_expression(&expr[idx + 2..]);
    }

    if expr.starts_with('!') {
        return !evaluate_expression(&expr[1..].trim_start());
    }

    if expr.starts_with('(') && expr.ends_with(')') {
        return evaluate_expression(&expr[1..expr.len() - 1].trim());
    }

    expr != "false" && !expr.is_empty()
}

fn find_operator(expr: &str, op: &str) -> Option<usize> {
    let mut depth = 0;
    for (i, c) in expr.char_indices() {
        match c {
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
        content_tokens: Option<Arc<[Token]>>
    ) -> Result<String> {
        let mut output = String::new();
        self.render_to_buf(tokens, vars, current_dir, page_dir, content_tokens, &mut output)?;
        Ok(output)
    }

    pub fn render_to_buf(
        &self, tokens: &[Token],
        vars: &mut HashMap<String, String>,
        current_dir: &Path,
        page_dir: &Path,
        content_tokens: Option<Arc<[Token]>>,
        output: &mut String
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
                        "include" => self.handle_include(params, vars, current_dir, page_dir, content_tokens.clone(), output)?,
                        "content" => self.handle_content(vars, page_dir, content_tokens.clone(), output)?,
                        "if" => {
                            let new_i = self.handle_conditional_block(tokens, i, vars, current_dir, page_dir, content_tokens.clone(), output)?;
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

    fn handle_echo(&self, params: &[(String, String)], vars: &HashMap<String, String>, output: &mut String) -> Result<()> {
        let var_name = get_param(params, "var").ok_or_else(|| anyhow!("echo missing 'var' parameter"))?;
        if let Some(val) = vars.get(var_name) {
            output.push_str(val);
        }
        Ok(())
    }

    fn handle_set(&self, params: &[(String, String)], vars: &mut HashMap<String, String>) -> Result<()> {
        let var_name = get_param(params, "var").ok_or_else(|| anyhow!("set missing 'var' parameter"))?;
        let val = get_param(params, "val").ok_or_else(|| anyhow!("set missing 'val' parameter"))?;
        let resolved = resolve_vars(val, vars);
        vars.insert(var_name.to_string(), resolved);
        Ok(())
    }

    fn handle_unset(&self, params: &[(String, String)], vars: &mut HashMap<String, String>) -> Result<()> {
        let var_name = get_param(params, "var").ok_or_else(|| anyhow!("unset missing 'var' parameter"))?;
        vars.remove(var_name);
        Ok(())
    }

    fn handle_include(&self, params: &[(String, String)], vars: &mut HashMap<String, String>, current_dir: &Path, page_dir: &Path, content_tokens: Option<Arc<[Token]>>, output: &mut String) -> Result<()> {
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
                let included_text = std::fs::read_to_string(&include_path)
                    .with_context(|| format!("Failed to read include file: {}", include_path.display()))?;
                let tokens: Arc<[Token]> = tokenize(&included_text)?.into();
                cache.insert(include_path.clone(), tokens.clone());
                tokens
            }
        };

        self.render_to_buf(&tokens, vars, include_path.parent().unwrap_or(current_dir), page_dir, content_tokens, output)?;
        Ok(())
    }

    fn handle_content(&self, vars: &mut HashMap<String, String>, page_dir: &Path, content_tokens: Option<Arc<[Token]>>, output: &mut String) -> Result<()> {
        if let Some(tokens) = content_tokens {
            self.render_to_buf(&tokens, vars, page_dir, page_dir, None, output)?;
            Ok(())
        } else {
            bail!("content tag used outside of template");
        }
    }

    fn handle_conditional_block(&self, tokens: &[Token], start_idx: usize, vars: &mut HashMap<String, String>, current_dir: &Path, page_dir: &Path, content_tokens: Option<Arc<[Token]>>, output: &mut String) -> Result<usize> {
        let mut blocks: Vec<(Option<String>, usize, usize)> = Vec::new(); // (Option<expr>, start_idx, end_idx)

        let first_tag = &tokens[start_idx];
        let (mut current_expr, mut current_start) = match first_tag {
            Token::Tag { name, params } if name == "if" => {
                (get_param(params, "expr").map(|e| resolve_vars(e, vars)), start_idx + 1)
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
                Token::Tag { name, params: next_params } if (name == "elif" || name == "else") && depth == 0 => {
                    blocks.push((current_expr, current_start, j));
                    if name == "else" {
                        current_expr = Some("true".to_string());
                    } else {
                        current_expr = get_param(next_params, "expr").map(|e| resolve_vars(e, vars));
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
                Some(e) => evaluate_expression(&e),
                None => true,
            };
            if should_render {
                self.render_to_buf(&tokens[start..end], vars, current_dir, page_dir, content_tokens, output)?;
                return Ok(next_i);
            }
        }

        Ok(next_i)
    }
}

fn get_param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

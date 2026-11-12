//! HTML → what we care about: title, image, price, stock and visible text.

use scraper::{ElementRef, Html, Node, Selector};
use serde::Serialize;
use serde_json::Value;

#[derive(Default, Debug, Clone, Serialize)]
pub struct Extracted {
    pub title: Option<String>,
    pub image: Option<String>,
    pub price: Option<f64>,
    pub currency: Option<String>,
    pub in_stock: Option<bool>,
    #[serde(skip)]
    pub text: String,
}

const MAX_TEXT: usize = 200_000;
const MAX_DEPTH: usize = 400;

pub fn extract(html: &str, selector: Option<&str>) -> Result<Extracted, String> {
    let doc = Html::parse_document(html);
    let mut out = Extracted::default();

    // 1. JSON-LD (schema.org Product / Offer): what most shops publish for Google.
    for script in doc.select(&sel(r#"script[type="application/ld+json"]"#)) {
        let raw: String = script.text().collect();
        if let Ok(v) = serde_json::from_str::<Value>(raw.trim())
            && let Some(p) = find_product(&v, 0)
        {
            read_product(p, &mut out);
            if out.price.is_some() {
                break;
            }
        }
    }

    // 2. Meta tags / microdata fallbacks.
    let meta = |q: &str| -> Option<String> {
        doc.select(&sel(q))
            .find_map(|e| e.value().attr("content").or(e.value().attr("href")))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    if out.price.is_none() {
        out.price = [
            r#"meta[property="product:price:amount"]"#,
            r#"meta[property="og:price:amount"]"#,
            r#"meta[itemprop="price"]"#,
        ]
        .iter()
        .find_map(|q| meta(q).and_then(|s| parse_price_str(&s)));
    }
    if out.currency.is_none() {
        out.currency = meta(r#"meta[property="product:price:currency"]"#)
            .or_else(|| meta(r#"meta[property="og:price:currency"]"#))
            .or_else(|| meta(r#"meta[itemprop="priceCurrency"]"#));
    }
    if out.in_stock.is_none() {
        out.in_stock = meta(r#"meta[property="product:availability"]"#)
            .or_else(|| meta(r#"[itemprop="availability"]"#))
            .and_then(|s| availability(&s));
    }
    if out.title.is_none() {
        out.title = meta(r#"meta[property="og:title"]"#).or_else(|| doc.select(&sel("title")).next().map(|t| squash(&t.text().collect::<String>())));
    }
    if out.image.is_none() {
        out.image = meta(r#"meta[property="og:image"]"#);
    }
    out.title = out.title.map(|t| t.chars().take(300).collect());

    // 3. Visible text, of the selector if given, else of <body>.
    let mut text = String::new();
    match selector.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let parsed = Selector::parse(s).map_err(|_| format!("Selector CSS inválido: {s}"))?;
            let mut any = false;
            for el in doc.select(&parsed) {
                any = true;
                visible_text(el, &mut text, 0);
                text.push('\n');
            }
            if !any {
                return Err(format!("El selector «{s}» no encontró nada en la página"));
            }
        }
        None => {
            if let Some(body) = doc.select(&sel("body")).next() {
                visible_text(body, &mut text, 0);
            }
        }
    }
    out.text = text.lines().map(squash).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
    if out.text.len() > MAX_TEXT {
        let mut cut = MAX_TEXT;
        while !out.text.is_char_boundary(cut) {
            cut -= 1;
        }
        out.text.truncate(cut);
    }
    Ok(out)
}

fn sel(s: &str) -> Selector {
    Selector::parse(s).expect("static selector")
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn visible_text(el: ElementRef, out: &mut String, depth: usize) {
    if depth > MAX_DEPTH || out.len() > MAX_TEXT * 2 {
        return;
    }
    for child in el.children() {
        match child.value() {
            Node::Text(t) => out.push_str(t),
            Node::Element(e) => {
                let name = e.name();
                if matches!(name, "script" | "style" | "noscript" | "template" | "svg" | "iframe" | "head") {
                    continue;
                }
                if let Some(c) = ElementRef::wrap(child) {
                    visible_text(c, out, depth + 1);
                }
                let inline = matches!(
                    name,
                    "a" | "b" | "i" | "em" | "strong" | "span" | "small" | "sup" | "sub" | "u" | "code" | "label" | "abbr" | "mark" | "time"
                );
                out.push(if inline { ' ' } else { '\n' });
            }
            _ => {}
        }
    }
}

/// Depth-first search for the first schema.org Product (also inside @graph and arrays).
fn find_product(v: &Value, depth: usize) -> Option<&Value> {
    if depth > 8 {
        return None;
    }
    match v {
        Value::Array(a) => a.iter().find_map(|x| find_product(x, depth + 1)),
        Value::Object(o) => {
            let is_product = match o.get("@type") {
                Some(Value::String(s)) => s.contains("Product"),
                Some(Value::Array(a)) => a.iter().any(|t| t.as_str().is_some_and(|s| s.contains("Product"))),
                _ => false,
            };
            if is_product && o.contains_key("offers") {
                return Some(v);
            }
            o.values().find_map(|x| find_product(x, depth + 1))
        }
        _ => None,
    }
}

fn read_product(p: &Value, out: &mut Extracted) {
    out.title = p.get("name").and_then(Value::as_str).map(squash);
    out.image = match p.get("image") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Array(a)) => a.first().and_then(|x| x.as_str().or(x.get("url").and_then(Value::as_str))).map(String::from),
        Some(o) => o.get("url").and_then(Value::as_str).map(String::from),
        None => None,
    };
    let offer = match p.get("offers") {
        Some(Value::Array(a)) => a.first(),
        other => other,
    };
    let Some(offer) = offer else { return };
    out.price = ["price", "lowPrice"]
        .iter()
        .find_map(|k| offer.get(*k).and_then(parse_price))
        .or_else(|| offer.pointer("/priceSpecification/price").and_then(parse_price));
    out.currency = offer.get("priceCurrency").and_then(Value::as_str).map(String::from);
    out.in_stock = offer.get("availability").and_then(Value::as_str).and_then(availability);
}

fn availability(s: &str) -> Option<bool> {
    let s = s.to_ascii_lowercase().replace([' ', '_', '-'], "");
    if ["outofstock", "soldout", "discontinued"].iter().any(|k| s.contains(k)) {
        Some(false)
    } else if ["instock", "limitedavailability", "onlineonly", "instoreonly", "preorder", "backorder"]
        .iter()
        .any(|k| s.contains(k))
    {
        Some(true)
    } else {
        None
    }
}

fn parse_price(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_price_str(s),
        _ => None,
    }
}

/// "27,639.00", "$1.234,56", "12,50", "6579" → number.
pub fn parse_price_str(s: &str) -> Option<f64> {
    let t: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.' || *c == ',').collect();
    let t = t.trim_matches(|c| c == '.' || c == ',');
    let norm = match (t.rfind('.'), t.rfind(',')) {
        (Some(d), Some(c)) if c > d => t.replace('.', "").replace(',', "."),
        (Some(_), Some(_)) => t.replace(',', ""),
        // Only commas: three digits after the last one means thousands, otherwise decimals.
        (None, Some(c)) if t.len() - c - 1 == 3 => t.replace(',', ""),
        (None, Some(_)) => t.replace(',', "."),
        (Some(_), None) if t.matches('.').count() > 1 => t.replace('.', ""),
        _ => t.to_string(),
    };
    norm.parse().ok().filter(|p: &f64| p.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices() {
        for (s, want) in [
            ("27,639.00", 27639.0),
            ("$1.234,56", 1234.56),
            ("27,639", 27639.0),
            ("12,50", 12.5),
            ("6579.00", 6579.0),
            ("1.234.567", 1234567.0),
            ("MXN 99.", 99.0),
        ] {
            assert_eq!(parse_price_str(s), Some(want), "{s}");
        }
        assert_eq!(parse_price_str("gratis"), None);
    }

    #[test]
    fn json_ld_product() {
        let html = r#"<html><head><title>Tienda</title>
        <script type="application/ld+json">{"@graph":[{"@type":"BreadcrumbList"},
          {"@type":"Product","name":"PC  Gamer","image":["https://x/y.jpg"],
           "offers":{"@type":"Offer","price":"27,639.00","priceCurrency":"MXN","availability":"https://schema.org/InStock"}}]}</script>
        </head><body><h1>PC Gamer</h1><script>var x=1</script><p>Hola <b>mundo</b></p></body></html>"#;
        let e = extract(html, None).unwrap();
        assert_eq!(e.title.as_deref(), Some("PC Gamer"));
        assert_eq!(e.price, Some(27639.0));
        assert_eq!(e.currency.as_deref(), Some("MXN"));
        assert_eq!(e.in_stock, Some(true));
        assert_eq!(e.image.as_deref(), Some("https://x/y.jpg"));
        assert_eq!(e.text, "PC Gamer\nHola mundo");
        assert_eq!(extract(html, Some("h1")).unwrap().text, "PC Gamer");
        assert!(extract(html, Some("#nope")).is_err());
    }

    #[test]
    fn meta_fallback() {
        let html = r#"<head><meta property="og:title" content="Cosa"><meta property="product:price:amount" content="499.90">
        <meta property="product:price:currency" content="MXN"><meta property="product:availability" content="out of stock"></head><body>x</body>"#;
        let e = extract(html, None).unwrap();
        assert_eq!((e.price, e.in_stock, e.title.as_deref()), (Some(499.9), Some(false), Some("Cosa")));
    }
}

use jaum_core::{Constraint, Enforce};

fn c(text: &str, pattern: Option<&str>) -> Constraint {
    Constraint {
        text: text.to_string(),
        enforce: Enforce::Hook,
        pattern: pattern.map(String::from),
    }
}

#[test]
fn hook_pattern_usa_pattern_explicito_quando_presente() {
    let c = c("nao tocar em src/legacy/", Some("src/(legacy|old)/"));
    assert_eq!(c.hook_pattern(), "src/(legacy|old)/");
}

#[test]
fn hook_pattern_deriva_caminho_do_texto() {
    let c = c("nao tocar em src/legacy/", None);
    // caminho extraído e com `.` escapado se houvesse
    assert_eq!(c.hook_pattern(), "src/legacy/");
}

#[test]
fn hook_pattern_escapa_ponto_em_caminho() {
    let c = c("nao editar config.toml", None);
    assert_eq!(c.hook_pattern(), r"config\.toml");
}

#[test]
fn hook_pattern_deriva_keyword_quando_nao_ha_caminho() {
    let c = c("nao rodar migration", None);
    // stopwords (nao, rodar) descartadas
    assert_eq!(c.hook_pattern(), "migration");
}

#[test]
fn hook_pattern_alterna_multiplas_palavras_significativas() {
    let c = c("nao usar reflection runtime", None);
    let p = c.hook_pattern();
    assert!(p.contains("reflection"));
    assert!(p.contains("runtime"));
    assert!(p.contains('|'));
}

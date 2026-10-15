use super::*;

fn texts(engine: &Engine, input: &str, n: usize) -> Vec<String> {
    engine
        .candidates(input, n)
        .into_iter()
        .map(|c| c.text)
        .collect()
}

/// The built-in data alone, loaded once for tests that do not learn.
fn shared() -> &'static Engine {
    static ENGINE: std::sync::OnceLock<Engine> = std::sync::OnceLock::new();
    ENGINE.get_or_init(|| Engine::load(None).unwrap())
}

fn first(engine: &Engine, input: &str) -> Candidate {
    engine.candidates(input, 1).remove(0)
}

/// Types `letters` into the composer.
fn type_in(composer: &mut Composer, letters: &str) {
    for c in letters.chars() {
        assert_eq!(composer.key(Key::Char(c)), Outcome::Consumed);
    }
}

/// Chooses `text` from the composer's pages, by its digit.
fn pick(composer: &mut Composer, text: &str) -> Outcome {
    for _ in 0..40 {
        let (page, _) = composer.page();
        if let Some(i) = page.iter().position(|t| t == text) {
            return composer.key(Key::Digit(i as u8 + 1));
        }
        composer.key(Key::PageDown);
    }
    panic!("{text} is not a candidate");
}

/// Prints load time, resident memory and candidates (`PY="nihao zg"` to
/// choose the input): `cargo test --release pinyin::tests::show -- --ignored --nocapture`.
#[test]
#[ignore]
fn show() {
    fn rss() -> String {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let line = status.lines().find(|l| l.starts_with("VmRSS"));
        line.unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("?")
            .to_string()
    }
    let before = rss();
    let t = std::time::Instant::now();
    let engine = Engine::load(None).unwrap();
    eprintln!("load {:?}, VmRSS {before} kB → {} kB", t.elapsed(), rss());
    let inputs = std::env::var("PY").unwrap_or(
        "nihao zg xian xi'an woshizhongguoren zhongg shi de zhon nihaoma fangan lv nve z".into(),
    );
    for input in inputs.split(' ') {
        let t = std::time::Instant::now();
        let c = engine.candidates(input, 20);
        let shown: Vec<String> = c
            .iter()
            .map(|c| format!("{}/{}", c.text, c.consumed))
            .collect();
        eprintln!("{input} ({:?}): {}", t.elapsed(), shown.join(" "));
    }
    // With PINYIN_USER=<path>: the first run fills a user dictionary there
    // (60 000 words, 20 000 pairs), the next one loads it.
    let Some(path) = std::env::var_os("PINYIN_USER").map(PathBuf::from) else {
        return;
    };
    drop(engine);
    if !path.exists() {
        let mut engine = Engine::load(Some(path.clone())).unwrap();
        let chars = engine.chars.clone();
        let word =
            |i: usize| -> String { [chars[i % 3000], chars[i / 3000 % 3000]].iter().collect() };
        let words: Vec<String> = (0..60_000).map(|i| word(i * 7)).collect();
        for (i, w) in words.iter().enumerate() {
            let previous = (i % 3 == 0).then(|| words[(i * 13) % words.len()].as_str());
            engine.learn(previous, w);
        }
        engine.save().unwrap();
        return;
    }
    let size = std::fs::metadata(&path).unwrap().len();
    let t = std::time::Instant::now();
    let engine = Engine::load(Some(path)).unwrap();
    eprintln!(
        "with {} user entries ({size} bytes): load {:?}, VmRSS {} kB; zg {:?}",
        engine.user.len(),
        t.elapsed(),
        rss(),
        texts(&engine, "zg", 3),
    );
}

#[test]
fn data_loads() {
    let engine = shared();
    assert!(engine.chars.len() > 40_000);
    assert!(engine.text_at.len() > 47_000);
    assert!(engine.names.contains(&"zhuang".to_string()));
    assert!(engine.candidates("", 9).is_empty());
    assert!(engine.candidates("ni hao", 9).is_empty());
    assert!(engine.candidates("i", 9).is_empty());
}

#[test]
fn full_pinyin_phrase() {
    let c = first(shared(), "nihao");
    assert_eq!(
        c,
        Candidate {
            text: "你好".into(),
            consumed: 5
        }
    );
    assert_eq!(first(shared(), "zhongguoren").text, "中国人");
    assert_eq!(first(shared(), "ZhongGuo").text, "中国");
}

#[test]
fn abbreviated_pinyin() {
    assert!(texts(shared(), "zg", 3).contains(&"中国".to_string()));
    assert_eq!(first(shared(), "zhongg").text, "中国");
    assert_eq!(first(shared(), "zgr").text, "中国人");
    assert_eq!(first(shared(), "zhonggr").text, "中国人");
    // Unfinished last syllable.
    assert!(texts(shared(), "zhon", 9).contains(&"中".to_string()));
    // Every candidate for a lone initial is one character.
    for c in shared().candidates("z", 50) {
        assert_eq!((c.text.chars().count(), c.consumed), (1, 1));
    }
}

#[test]
fn splits_and_apostrophe() {
    let engine = shared();
    // One syllable first, the xi + an split still offered.
    let all = engine.candidates("xian", 300);
    assert_eq!(all[0].consumed, 4);
    assert!(all.iter().any(|c| c.text == "先" && c.consumed == 4));
    assert!(all.iter().any(|c| c.text == "西" && c.consumed == 2));
    // The apostrophe forces the split.
    let all = engine.candidates("xi'an", 300);
    assert!(all.iter().all(|c| c.consumed == 2 || c.consumed == 5));
    assert!(texts(engine, "xi'an", 9).contains(&"西".to_string()));
    // fang'an and fan'gan.
    assert!(
        engine
            .candidates("fang'an", 9)
            .iter()
            .all(|c| c.consumed == 4)
    );
    assert!(
        engine
            .candidates("fan'gan", 9)
            .iter()
            .all(|c| c.consumed == 3)
    );
}

#[test]
fn whole_sentence() {
    let mut engine = Engine::load(None).unwrap();
    let c = first(&engine, "woshizhongguoren");
    assert_eq!(c.consumed, 16);
    assert_eq!(c.text.chars().count(), 5);
    assert!(c.text.ends_with("中国人"), "{}", c.text);
    let c = first(&engine, "nihaoma");
    assert_eq!((c.text.chars().count(), c.consumed), (3, 7));
    assert!(c.text.starts_with("你好"), "{}", c.text);
    // The data has no frequencies: once the user chose 我 and 是, they
    // are used.
    engine.learn(None, "我");
    engine.learn(None, "是");
    assert_eq!(first(&engine, "woshizhongguoren").text, "我是中国人");
}

#[test]
fn character_order() {
    let engine = shared();
    // First-listed readings before secondary ones: 地 is dì first.
    let de = texts(engine, "de", 20);
    let at = |c: &str| de.iter().position(|t| t == c).unwrap();
    assert!(at("的") < at("地"), "{de:?}");
    assert!(texts(engine, "di", 3).contains(&"地".to_string()));
    // Common characters before rare and traditional ones.
    let shi = texts(engine, "shi", 18);
    assert!(shi.contains(&"是".to_string()), "{shi:?}");
    assert!(!shi.contains(&"時".to_string()));
}

#[test]
fn tones_and_u_umlaut() {
    let engine = shared();
    assert!(texts(engine, "lv", 3).contains(&"绿".to_string()));
    assert!(texts(engine, "nv", 3).contains(&"女".to_string()));
    assert_eq!(first(engine, "nve").text, "虐");
    assert_eq!(first(engine, "nue").text, "虐");
    assert_eq!(first(engine, "lue").text, "略");
    // ü after j, q, x, y is written u; v is accepted too.
    assert_eq!(texts(engine, "jv", 20), texts(engine, "ju", 20));
    assert_eq!(texts(engine, "xvyv", 9), texts(engine, "xuyu", 9));
    assert_eq!(first(engine, "fengyv").text, "风雨");
}

#[test]
fn learning_reorders() {
    let mut engine = Engine::load(None).unwrap();
    assert_ne!(first(&engine, "shi").text, "是");
    engine.learn(None, "是");
    assert_eq!(first(&engine, "shi").text, "是");
    engine.learn(None, "时");
    engine.learn(None, "时");
    assert_eq!(texts(&engine, "shi", 2), ["时", "是"]);
    // A word not in the data is found after it was chosen, by full and
    // abbreviated pinyin.
    assert!(!texts(&engine, "xian", 20).contains(&"西安".to_string()));
    engine.learn(None, "西安");
    assert_eq!(first(&engine, "xi'an").text, "西安");
    assert_eq!(first(&engine, "xian").text, "西安");
    assert_eq!(first(&engine, "x'an").text, "西安");
    // Polyphones: 重庆 by chong as well as zhong.
    engine.learn(None, "重庆");
    assert_eq!(first(&engine, "chongqing").text, "重庆");
    assert_eq!(first(&engine, "cq").text, "重庆");
}

#[test]
fn association() {
    let mut engine = Engine::load(None).unwrap();
    assert!(engine.follow("你好", 9).is_empty());
    engine.learn(Some("你好"), "世界");
    engine.learn(Some("你好"), "吗");
    engine.learn(Some("你好"), "吗");
    assert_eq!(engine.follow("你好", 9), ["吗", "世界"]);
    assert_eq!(engine.follow("你好", 1), ["吗"]);
    assert!(engine.follow("世界", 9).is_empty());
}

#[test]
fn persistence_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pinyin").join("user.dict");
    let mut engine = Engine::load(Some(path.clone())).unwrap();
    assert!(path.exists(), "created when missing");
    engine.learn(None, "西安");
    engine.learn(Some("西安"), "古城");
    engine.learn(Some("西安"), "古城");
    engine.learn(None, "bad\tword");
    engine.save().unwrap();
    let saved = std::fs::read_to_string(&path).unwrap();
    assert!(!saved.contains("bad"));
    assert_eq!(
        std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
        1
    );

    let mut engine = Engine::load(Some(path.clone())).unwrap();
    assert_eq!(engine.user.count("西安"), 1);
    assert_eq!(engine.user.count("古城"), 2);
    assert_eq!(engine.follow("西安", 9), ["古城"]);
    assert_eq!(first(&engine, "xian").text, "西安");
    // The clock continues: a new word is newer than the loaded ones.
    engine.learn(None, "长安");
    let stamp = |w: &str| engine.user.words[w].stamp;
    assert!(stamp("长安") > stamp("古城"));
}

#[test]
fn cap_evicts_least_recently_used() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("user.dict");
    let mut engine = Engine::load(Some(path.clone())).unwrap();
    engine.user.cap = 100;
    let words: Vec<String> = (0..150).map(|i| format!("词{i}")).collect();
    engine.learn(None, "西安");
    for (i, word) in words.iter().enumerate() {
        engine.learn(None, word);
        if i % 10 == 0 {
            // Keep 西安 recent.
            engine.learn(None, "西安");
        }
    }
    assert!(engine.user.len() <= 100);
    assert!(engine.user.count("西安") > 0);
    assert_eq!(engine.user.count("词0"), 0);
    assert_eq!(engine.user.count("词149"), 1);
    // Evicted words leave the index too.
    assert!(
        engine
            .user_keys
            .values()
            .flatten()
            .all(|w| engine.user.count(w) > 0)
    );
    engine.save().unwrap();
    let mut reloaded = Engine::load(Some(path)).unwrap();
    reloaded.user.cap = 100;
    assert_eq!(reloaded.user.len(), engine.user.len());
    // Pairs count against the cap too.
    for word in &words {
        reloaded.learn(Some("西安"), word);
    }
    assert!(reloaded.user.len() <= 100);
}

#[test]
fn composer_chooses_parts_and_learns_the_whole() {
    let mut composer = Composer::new(Engine::load(None).unwrap());
    assert!(composer.is_empty());
    type_in(&mut composer, "nihaoma");
    assert_eq!(composer.preedit(), "nihaoma");
    assert_eq!(pick(&mut composer, "你好"), Outcome::Consumed);
    assert_eq!(composer.preedit(), "你好 ma");
    assert!(!composer.page().0.is_empty());
    assert_eq!(pick(&mut composer, "吗"), Outcome::Commit("你好吗".into()));
    let engine = composer.engine();
    assert_eq!(engine.user.count("你好"), 1);
    assert_eq!(engine.user.count("吗"), 1);
    assert_eq!(first(engine, "nihaoma").text, "你好吗");
    assert_eq!(engine.follow("你好", 9), ["吗"]);
}

#[test]
fn composer_apostrophe_and_backspace() {
    let mut composer = Composer::new(Engine::load(None).unwrap());
    type_in(&mut composer, "xi'an");
    assert_eq!(pick(&mut composer, "西"), Outcome::Consumed);
    assert_eq!(composer.preedit(), "西 an");
    // Backspace removes letters, then un-chooses.
    composer.key(Key::Backspace);
    assert_eq!(composer.preedit(), "西 a");
    composer.key(Key::Backspace);
    assert_eq!(composer.preedit(), "西");
    assert!(composer.page().0.is_empty());
    composer.key(Key::Backspace);
    assert_eq!(composer.preedit(), "xi'");
    assert!(!composer.page().0.is_empty());
    type_in(&mut composer, "an");
    assert_eq!(pick(&mut composer, "西"), Outcome::Consumed);
    assert_eq!(pick(&mut composer, "安"), Outcome::Commit("西安".into()));
    // The composed word is learned.
    type_in(&mut composer, "xian");
    assert_eq!(composer.page().0[0], "西安");
    assert_eq!(composer.key(Key::Space), Outcome::Commit("西安".into()));
}

#[test]
fn composer_keys() {
    let mut composer = Composer::new(Engine::load(None).unwrap());
    // Empty: only letters are taken.
    for key in [
        Key::Space,
        Key::Enter,
        Key::Backspace,
        Key::Digit(1),
        Key::Up,
        Key::Escape,
    ] {
        assert_eq!(composer.key(key), Outcome::Pass);
    }
    assert_eq!(composer.key(Key::Char('\'')), Outcome::Pass);
    assert_eq!(composer.key(Key::Char('A')), Outcome::Pass);
    assert_eq!(composer.key(Key::Char(',')), Outcome::Pass);
    // Enter commits the raw letters, Escape clears.
    type_in(&mut composer, "nihao");
    assert_eq!(composer.key(Key::Enter), Outcome::Commit("nihao".into()));
    assert!(composer.is_empty());
    type_in(&mut composer, "nihao");
    assert_eq!(composer.key(Key::Escape), Outcome::Consumed);
    assert!(composer.is_empty());
    assert_eq!(composer.preedit(), "");
    // Highlight and pages.
    type_in(&mut composer, "ji");
    let first_page: Vec<String> = composer.page().0.to_vec();
    assert_eq!(first_page.len(), 9);
    assert_eq!(composer.page().1, 0);
    composer.key(Key::Right);
    composer.key(Key::Down);
    assert_eq!(composer.page().1, 2);
    composer.key(Key::Left);
    assert_eq!(composer.page().1, 1);
    assert_eq!(
        composer.key(Key::Space),
        Outcome::Commit(first_page[1].clone())
    );
    type_in(&mut composer, "yi");
    let first_page: Vec<String> = composer.page().0.to_vec();
    composer.key(Key::PageDown);
    let second_page: Vec<String> = composer.page().0.to_vec();
    assert_ne!(second_page, first_page);
    for _ in 0..9 {
        composer.key(Key::Left);
    }
    assert_eq!(composer.page().0, &first_page[..]);
    assert_eq!(composer.page().1, 0);
    composer.key(Key::PageDown);
    composer.key(Key::PageUp);
    assert_eq!(composer.page().0, &first_page[..]);
    // A digit past the page is ignored; another key commits what is typed.
    assert_eq!(composer.key(Key::Digit(0)), Outcome::Consumed);
    assert_eq!(composer.key(Key::Char(',')), Outcome::Commit("yi,".into()));
}

#[test]
fn composer_association() {
    let mut composer = Composer::new(Engine::load(None).unwrap());
    type_in(&mut composer, "nihao");
    assert_eq!(composer.key(Key::Space), Outcome::Commit("你好".into()));
    // Nothing followed 你好 yet.
    assert!(composer.is_empty());
    type_in(&mut composer, "shijie");
    let shijie = composer.page().0[0].clone();
    assert_eq!(composer.key(Key::Digit(1)), Outcome::Commit(shijie.clone()));
    // Now that word follows 你好.
    type_in(&mut composer, "nihao");
    assert_eq!(composer.key(Key::Space), Outcome::Commit("你好".into()));
    assert!(!composer.is_empty());
    assert_eq!(composer.preedit(), "");
    assert_eq!(composer.page(), (&[shijie.clone()][..], 0));
    assert_eq!(composer.key(Key::Digit(1)), Outcome::Commit(shijie.clone()));
    // Other keys drop the association and pass; a letter starts afresh.
    type_in(&mut composer, "nihao");
    composer.key(Key::Space);
    assert_eq!(composer.key(Key::Enter), Outcome::Pass);
    assert!(composer.is_empty());
    type_in(&mut composer, "nihao");
    composer.key(Key::Space);
    type_in(&mut composer, "ma");
    assert_eq!(composer.preedit(), "ma");
}

#[test]
fn composer_saves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("user.dict");
    let mut composer = Composer::new(Engine::load(Some(path.clone())).unwrap());
    type_in(&mut composer, "nihao");
    composer.key(Key::Space);
    composer.save().unwrap();
    let engine = Engine::load(Some(path)).unwrap();
    assert_eq!(engine.user.count("你好"), 1);
}

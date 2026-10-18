//! The recipe library: an ordered list of validated recipes with display metadata.
//!
//! Stored as one `recipes.json` (small, atomic to rewrite). Each entry wraps a
//! [`xbloom_core::Recipe`] — the same schema as the `xbloom` CLI's YAML files — so recipes
//! move freely between the CLI, the admin panel and the screen. Import accepts YAML or JSON:
//! a single recipe, a list, or this store's own export (`{"recipes": [...]}`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use xbloom_core::recipe::RawRecipe;
use xbloom_core::Recipe;

use crate::util::{new_id, now_unix, read_json, write_json};

/// Most recipes the store holds (the screen shows them as cards).
pub const MAX_RECIPES: usize = 200;

/// Display metadata the screen keeps alongside a recipe.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct RecipeMeta {
    /// Pinned to the front.
    pub favorite: bool,
    /// Hidden from the brew screen (still editable in the admin panel).
    pub hidden: bool,
    /// Card colour, `#rrggbb` (`None` = theme accent).
    pub color: Option<String>,
    /// Created, unix seconds.
    pub created: u64,
    /// Last edited, unix seconds.
    pub updated: u64,
    /// Completed brews.
    pub brews: u32,
    /// Last completed brew, unix seconds.
    pub last_brewed: Option<u64>,
}

/// A recipe in the library.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredRecipe {
    /// Stable id.
    pub id: String,
    /// Display metadata.
    #[serde(default)]
    pub meta: RecipeMeta,
    /// The recipe itself (validated on load and save).
    pub recipe: Recipe,
}

#[derive(Serialize, Deserialize, Default)]
struct File {
    #[serde(default)]
    recipes: Vec<StoredRecipe>,
    /// The newest starter set this library has been given (see [`STARTERS`]). Missing in
    /// files from before sets were numbered, which had set 1.
    #[serde(default)]
    starters: u32,
}

/// The library.
#[derive(Debug)]
pub struct RecipeStore {
    path: PathBuf,
    items: Vec<StoredRecipe>,
    starters: u32,
}

/// The starter recipes shipped with the repo (`recipes/*.yaml`), each with the starter set
/// it arrived in. A new library gets them all; an existing one gets only the sets added
/// since, so an update brings the tea starters to a library made before them.
pub const STARTERS: [(u32, &str); 6] = [
    (1, include_str!("../../../../recipes/example-washed.yaml")),
    (1, include_str!("../../../../recipes/example-natural.yaml")),
    (1, include_str!("../../../../recipes/example-iced.yaml")),
    (2, include_str!("../../../../recipes/example-tea-green.yaml")),
    (2, include_str!("../../../../recipes/example-tea-black.yaml")),
    (2, include_str!("../../../../recipes/example-tea-oolong.yaml")),
];

/// The newest starter set.
const STARTER_SET: u32 = 2;

/// A tea recipe (`kind: tea`). The screen files it under Tea and brews it with the grinder
/// off.
pub fn is_tea(recipe: &Recipe) -> bool {
    recipe.kind.as_deref().is_some_and(|k| k.trim().eq_ignore_ascii_case("tea"))
}

/// The recipe's own validation plus the screen's rules: a tea recipe never grinds (its
/// leaves would go through the grinder).
pub fn check(recipe: &Recipe) -> Result<(), String> {
    recipe.validate().map_err(|e| e.0)?;
    if is_tea(recipe) && !recipe.no_grind() {
        return Err("a tea recipe brews with the grinder off: set grind to 0".into());
    }
    Ok(())
}

/// Parse recipe text: YAML or JSON, one recipe, a list, or `{"recipes": [...]}`.
///
/// Returns each item's result separately so one bad recipe doesn't sink a whole import.
pub fn parse_recipes(text: &str) -> Result<Vec<Result<Recipe, String>>, String> {
    let value: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => serde_norway::from_str(text).map_err(|e| format!("not valid YAML or JSON: {e}"))?,
    };
    let items: Vec<serde_json::Value> = match value {
        serde_json::Value::Array(a) => a,
        serde_json::Value::Object(ref o) if o.get("recipes").is_some_and(|r| r.is_array()) => o["recipes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            // Accept this store's export shape ({id, meta, recipe}) as well as bare recipes.
            .map(|v| v.get("recipe").cloned().unwrap_or(v))
            .collect(),
        serde_json::Value::Object(_) => vec![value],
        _ => return Err("expected a recipe (a mapping) or a list of recipes".into()),
    };
    Ok(items
        .into_iter()
        .map(|v| {
            serde_json::from_value::<RawRecipe>(v)
                .map_err(|e| e.to_string())
                .and_then(|raw| Recipe::from_raw(raw).map_err(|e| e.0))
        })
        .collect())
}

/// A recipe as the `xbloom` CLI's YAML.
pub fn to_yaml(recipe: &Recipe) -> String {
    serde_norway::to_string(recipe).unwrap_or_default()
}

impl RecipeStore {
    /// Open (or create) the store at `path`. A new library gets the starter recipes; an
    /// existing one gets any starter set added since it was made.
    pub fn open(path: PathBuf) -> Self {
        let (items, seeded, fresh) = match read_json::<File>(&path) {
            Ok(Some(f)) => (f.recipes, f.starters.max(1), false),
            Ok(None) => (Vec::new(), 0, true),
            Err(e) => {
                log::error!("recipes: {} is unreadable ({e}); starting empty, keeping a backup", path.display());
                let _ = std::fs::copy(&path, path.with_extension("json.bad"));
                (Vec::new(), STARTER_SET, false)
            }
        };
        let mut store = Self { path, items, starters: seeded };
        if seeded < STARTER_SET {
            for (set, text) in STARTERS {
                if set <= seeded {
                    continue;
                }
                if let Ok(list) = parse_recipes(text) {
                    for r in list.into_iter().flatten() {
                        let _ = store.add(r, RecipeMeta::default());
                    }
                }
            }
            if fresh {
                if let Some(first) = store.items.first_mut() {
                    first.meta.favorite = true;
                }
            }
            store.starters = STARTER_SET;
            store.save();
        }
        store
    }

    fn save(&self) {
        if let Err(e) = write_json(&self.path, &File { recipes: self.items.clone(), starters: self.starters }) {
            log::error!("recipes: save failed: {e}");
        }
    }

    /// All recipes in display order (favourites first, then the stored order).
    pub fn list(&self) -> Vec<StoredRecipe> {
        let mut v = self.items.clone();
        v.sort_by_key(|r| !r.meta.favorite);
        v
    }

    /// Recipes shown on the brew screen (not hidden).
    pub fn visible(&self) -> Vec<StoredRecipe> {
        self.list().into_iter().filter(|r| !r.meta.hidden).collect()
    }

    /// One recipe by id.
    pub fn get(&self, id: &str) -> Option<StoredRecipe> {
        self.items.iter().find(|r| r.id == id).cloned()
    }

    /// Add a (validated) recipe; returns the new entry.
    pub fn add(&mut self, recipe: Recipe, mut meta: RecipeMeta) -> Result<StoredRecipe, String> {
        check(&recipe)?;
        if self.items.len() >= MAX_RECIPES {
            return Err(format!("the library is full ({MAX_RECIPES} recipes)"));
        }
        let now = now_unix();
        meta.created = now;
        meta.updated = now;
        let entry = StoredRecipe { id: new_id(), meta, recipe };
        self.items.push(entry.clone());
        self.save();
        Ok(entry)
    }

    /// Replace a recipe (and optionally its metadata).
    pub fn update(&mut self, id: &str, recipe: Recipe, meta: Option<RecipeMeta>) -> Result<StoredRecipe, String> {
        check(&recipe)?;
        let entry = self.items.iter_mut().find(|r| r.id == id).ok_or("no such recipe")?;
        entry.recipe = recipe;
        if let Some(m) = meta {
            // Counters are owned by the device, not the editor.
            entry.meta = RecipeMeta {
                created: entry.meta.created,
                brews: entry.meta.brews,
                last_brewed: entry.meta.last_brewed,
                ..m
            };
        }
        entry.meta.updated = now_unix();
        let out = entry.clone();
        self.save();
        Ok(out)
    }

    /// Change only the metadata (favourite, hidden, colour).
    pub fn update_meta(&mut self, id: &str, f: impl FnOnce(&mut RecipeMeta)) -> Option<StoredRecipe> {
        let entry = self.items.iter_mut().find(|r| r.id == id)?;
        f(&mut entry.meta);
        let out = entry.clone();
        self.save();
        Some(out)
    }

    /// Delete a recipe. Returns whether it existed.
    pub fn delete(&mut self, id: &str) -> bool {
        let before = self.items.len();
        self.items.retain(|r| r.id != id);
        let removed = self.items.len() != before;
        if removed {
            self.save();
        }
        removed
    }

    /// Duplicate a recipe as "<name> copy".
    pub fn duplicate(&mut self, id: &str) -> Result<StoredRecipe, String> {
        let src = self.get(id).ok_or("no such recipe")?;
        let mut r = src.recipe;
        r.name = format!("{} copy", r.name);
        self.add(r, RecipeMeta { color: src.meta.color, ..RecipeMeta::default() })
    }

    /// Reorder by a list of ids (unknown ids ignored; missing ones keep their relative order
    /// at the end).
    pub fn reorder(&mut self, ids: &[String]) {
        let mut out = Vec::with_capacity(self.items.len());
        for id in ids {
            if let Some(pos) = self.items.iter().position(|r| &r.id == id) {
                out.push(self.items.remove(pos));
            }
        }
        out.append(&mut self.items);
        self.items = out;
        self.save();
    }

    /// Count a completed brew.
    pub fn record_brew(&mut self, id: &str) {
        if let Some(e) = self.items.iter_mut().find(|r| r.id == id) {
            e.meta.brews += 1;
            e.meta.last_brewed = Some(now_unix());
            self.save();
        }
    }

    /// Import recipe text (see [`parse_recipes`]). Returns `(added, errors)`.
    pub fn import(&mut self, text: &str) -> Result<(Vec<StoredRecipe>, Vec<String>), String> {
        let mut added = Vec::new();
        let mut errors = Vec::new();
        for (i, r) in parse_recipes(text)?.into_iter().enumerate() {
            match r.and_then(|r| self.add(r, RecipeMeta::default())) {
                Ok(e) => added.push(e),
                Err(e) => errors.push(format!("recipe #{}: {e}", i + 1)),
            }
        }
        Ok((added, errors))
    }

    /// Replace the whole library (restore from a backup).
    pub fn replace_all(&mut self, items: Vec<StoredRecipe>) -> Result<(), String> {
        for r in &items {
            check(&r.recipe).map_err(|e| format!("{}: {e}", r.recipe.name))?;
        }
        self.items = items.into_iter().take(MAX_RECIPES).collect();
        self.save();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_starters_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("recipes.json");
        let mut s = RecipeStore::open(p.clone());
        assert_eq!(s.list().len(), STARTERS.len());
        assert!(s.list()[0].meta.favorite);
        assert_eq!(s.list().iter().filter(|r| is_tea(&r.recipe)).count(), 3);
        let id = s.list()[1].id.clone();
        s.update_meta(&id, |m| m.hidden = true);
        let s2 = RecipeStore::open(p);
        assert_eq!(s2.visible().len(), STARTERS.len() - 1);
        assert_eq!(s2.list().len(), STARTERS.len());
    }

    #[test]
    fn a_library_from_before_the_tea_starters_gets_them_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("recipes.json");
        // A file written before starter sets were numbered: the coffee starters only.
        let coffee: Vec<StoredRecipe> = STARTERS[..3]
            .iter()
            .map(|(_, t)| StoredRecipe {
                id: new_id(),
                meta: RecipeMeta::default(),
                recipe: parse_recipes(t).unwrap().remove(0).unwrap(),
            })
            .collect();
        write_json(&p, &serde_json::json!({ "recipes": coffee })).unwrap();
        let s = RecipeStore::open(p.clone());
        assert_eq!(s.list().len(), 6);
        assert!(!s.list().iter().any(|r| r.meta.favorite), "an existing library keeps its favourites");
        // Deleting a starter doesn't bring it back.
        let mut s = RecipeStore::open(p.clone());
        let tea = s.list().into_iter().find(|r| is_tea(&r.recipe)).unwrap();
        s.delete(&tea.id);
        assert_eq!(RecipeStore::open(p).list().len(), 5);
    }

    #[test]
    fn tea_never_grinds() {
        let mut r = parse_recipes(STARTERS[3].1).unwrap().remove(0).unwrap();
        assert!(is_tea(&r) && check(&r).is_ok());
        r.grind = 40;
        assert!(check(&r).unwrap_err().contains("grinder off"));
        r.kind = Some("custom".into());
        assert!(check(&r).is_ok());
    }

    #[test]
    fn import_yaml_list_and_reports_bad_items() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = RecipeStore::open(dir.path().join("r.json"));
        let yaml = format!("- {}\n- {{name: bad, dose_g: 99, grind: 1, pours: []}}\n", "{name: A, dose_g: 15, grind: 50, pours: [{ml: 40, temp_c: 92, rpm: 100}, {ml: 200, temp_c: 92, rpm: 100}]}");
        let (added, errors) = s.import(&yaml).unwrap();
        assert_eq!(added.len(), 1);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("non-empty"));
        // A store export re-imports.
        let export = serde_json::json!({"recipes": s.list()}).to_string();
        let (again, errs) = s.import(&export).unwrap();
        assert_eq!((again.len(), errs.len()), (STARTERS.len() + 1, 0));
    }

    #[test]
    fn yaml_export_matches_cli_schema() {
        let r = parse_recipes(STARTERS[0].1).unwrap().remove(0).unwrap();
        let y = to_yaml(&r);
        assert!(y.starts_with("name: Example Washed\ndose_g: 16\ngrind: 62\nratio: 15\n"), "{y}");
        let back = parse_recipes(&y).unwrap().remove(0).unwrap();
        assert_eq!(back, r);
    }

    #[test]
    fn reorder_and_duplicate() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = RecipeStore::open(dir.path().join("r.json"));
        let ids: Vec<String> = s.items.iter().map(|r| r.id.clone()).collect();
        s.reorder(&[ids[2].clone(), ids[0].clone()]);
        let mut want = vec![ids[2].clone(), ids[0].clone(), ids[1].clone()];
        want.extend_from_slice(&ids[3..]);
        assert_eq!(s.items.iter().map(|r| r.id.clone()).collect::<Vec<_>>(), want);
        let d = s.duplicate(&ids[0]).unwrap();
        assert!(d.recipe.name.ends_with(" copy"));
    }
}

#![allow(clippy::map_entry)]

use super::{Pair, WithFirstLastIterator, Word, BPE};
use crate::parallelism::*;
use crate::tokenizer::{AddedToken, Result, Trainer};
use crate::utils::progress::{ProgressBar, ProgressStyle};
use ahash::{AHashMap, AHashSet};
use compact_str::CompactString;
use dary_heap::OctonaryHeap;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

#[derive(Debug, Eq)]
struct Merge {
    pair: Pair,
    count: u64,
    pos: AHashSet<usize>,
}
impl PartialEq for Merge {
    fn eq(&self, other: &Self) -> bool {
        self.count == other.count && self.pair == other.pair
    }
}
impl PartialOrd for Merge {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Merge {
    fn cmp(&self, other: &Self) -> Ordering {
        if self.count != other.count {
            self.count.cmp(&other.count)
        } else {
            // Here we want ascending order
            other.pair.cmp(&self.pair)
        }
    }
}

struct Config {
    min_frequency: u64,
    vocab_size: usize,
    show_progress: bool,
    special_tokens: Vec<AddedToken>,
    limit_alphabet: Option<usize>,
    initial_alphabet: AHashSet<char>,
    continuing_subword_prefix: Option<String>,
    end_of_word_suffix: Option<String>,
    max_token_length: Option<usize>,
}

/// A `BpeTrainerBuilder` can be used to create a `BpeTrainer` with a custom
/// configuration.
pub struct BpeTrainerBuilder {
    config: Config,
}

impl Default for BpeTrainerBuilder {
    fn default() -> Self {
        Self {
            config: Config {
                min_frequency: 0,
                vocab_size: 30000,
                show_progress: true,
                special_tokens: vec![],
                limit_alphabet: None,
                initial_alphabet: AHashSet::new(),
                continuing_subword_prefix: None,
                end_of_word_suffix: None,
                max_token_length: None,
            },
        }
    }
}

impl BpeTrainerBuilder {
    /// Constructs a new `BpeTrainerBuilder`
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the expected minimum frequency
    #[must_use]
    pub fn min_frequency(mut self, frequency: u64) -> Self {
        self.config.min_frequency = frequency;
        self
    }

    /// Set the vocabulary size
    #[must_use]
    pub fn vocab_size(mut self, size: usize) -> Self {
        self.config.vocab_size = size;
        self
    }

    /// Set whether to show progress
    #[must_use]
    pub fn show_progress(mut self, show: bool) -> Self {
        self.config.show_progress = show;
        self
    }

    /// Set the special tokens
    #[must_use]
    pub fn special_tokens(mut self, tokens: Vec<AddedToken>) -> Self {
        self.config.special_tokens = tokens;
        self
    }

    /// Set whether to limit the alphabet
    #[must_use]
    pub fn limit_alphabet(mut self, limit: usize) -> Self {
        self.config.limit_alphabet = Some(limit);
        self
    }

    /// Set the initial alphabet
    #[must_use]
    pub fn initial_alphabet(mut self, alphabet: AHashSet<char>) -> Self {
        self.config.initial_alphabet = alphabet;
        self
    }

    /// Set the continuing_subword_prefix
    #[must_use]
    pub fn continuing_subword_prefix(mut self, prefix: String) -> Self {
        self.config.continuing_subword_prefix = Some(prefix);
        self
    }

    /// Set the end_of_word_suffix
    #[must_use]
    pub fn end_of_word_suffix(mut self, suffix: String) -> Self {
        self.config.end_of_word_suffix = Some(suffix);
        self
    }
    /// Set max_token_length
    #[must_use]
    pub fn max_token_length(mut self, max_token_length: Option<usize>) -> Self {
        self.config.max_token_length = max_token_length;
        self
    }

    /// Constructs the final BpeTrainer
    pub fn build(self) -> BpeTrainer {
        BpeTrainer {
            min_frequency: self.config.min_frequency,
            vocab_size: self.config.vocab_size,
            show_progress: self.config.show_progress,
            special_tokens: self.config.special_tokens,
            limit_alphabet: self.config.limit_alphabet,
            initial_alphabet: self.config.initial_alphabet,
            continuing_subword_prefix: self.config.continuing_subword_prefix,
            end_of_word_suffix: self.config.end_of_word_suffix,
            max_token_length: self.config.max_token_length,
            words: AHashMap::new(),
        }
    }
}

/// In charge of training a `BPE` model
///
/// # Examples
///
/// ```
/// use tokenizers::tokenizer::Trainer;
/// use tokenizers::models::bpe::{BPE, BpeTrainer};
///
/// let sequences = vec![ "Hello", "World" ];
///
/// let mut trainer = BpeTrainer::default();
/// trainer.feed(sequences.iter(), |s| Ok(vec![s.to_owned()]));
///
/// let mut model = BPE::default();
/// let special_tokens = trainer.train(&mut model).unwrap();
/// ```
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq)]
pub struct BpeTrainer {
    /// The minimum frequency a pair must have to produce a merge operation
    pub min_frequency: u64,
    /// The target vocabulary size
    pub vocab_size: usize,
    /// Whether to show progress while training
    pub show_progress: bool,
    /// A list of special tokens that the model should know of
    pub special_tokens: Vec<AddedToken>,
    /// Whether to limit the number of initial tokens that can be kept before computing merges
    pub limit_alphabet: Option<usize>,
    /// The initial alphabet we want absolutely to include. This allows to cover
    /// some characters that are not necessarily in the training set
    pub initial_alphabet: AHashSet<char>,
    /// An optional prefix to use on any subword that exist only behind another one
    pub continuing_subword_prefix: Option<String>,
    /// An optional suffix to characterize and end-of-word subword
    pub end_of_word_suffix: Option<String>,
    /// An optional parameter to limit the max length of any single token
    pub max_token_length: Option<usize>,

    words: AHashMap<CompactString, u64>,
}

impl Default for BpeTrainer {
    fn default() -> Self {
        Self::builder().build()
    }
}

impl BpeTrainer {
    pub fn new(min_frequency: u64, vocab_size: usize) -> Self {
        Self {
            min_frequency,
            vocab_size,
            ..Default::default()
        }
    }

    pub fn builder() -> BpeTrainerBuilder {
        BpeTrainerBuilder::new()
    }

    /// Setup a progress bar if asked to show progress
    fn setup_progress(&self) -> Option<ProgressBar> {
        if self.show_progress {
            let p = ProgressBar::new(0);
            p.set_style(
                ProgressStyle::default_bar()
                    .template("[{elapsed_precise}] {msg:<30!} {wide_bar} {pos:<9!}/{len:>9!}")
                    .expect("Invalid progress template"),
            );
            Some(p)
        } else {
            None
        }
    }

    /// Set the progress bar in the finish state
    fn finalize_progress(&self, p: &Option<ProgressBar>, final_len: usize) {
        if let Some(p) = p {
            p.set_length(final_len as u64);
            p.finish();
            println!();
        }
    }

    /// Update the progress bar with the new provided length and message
    fn update_progress(&self, p: &Option<ProgressBar>, len: usize, message: &'static str) {
        if let Some(p) = p {
            p.set_message(message);
            p.set_length(len as u64);
            p.reset();
        }
    }

    /// Add the provided special tokens to the initial vocabulary
    fn add_special_tokens(
        &self,
        w2id: &mut AHashMap<CompactString, u32>,
        id2w: &mut Vec<CompactString>,
    ) {
        for token in &self.special_tokens {
            // get hash of content
            if !w2id.contains_key(&CompactString::from(&token.content)) {
                id2w.push(CompactString::from(&token.content));
                w2id.insert(CompactString::from(&token.content), (id2w.len() - 1) as u32);
            }
        }
    }

    /// Compute the initial alphabet and limit it if relevant
    fn compute_alphabet(
        &self,
        wc: &AHashMap<CompactString, u64>,
        w2id: &mut AHashMap<CompactString, u32>,
        id2w: &mut Vec<CompactString>,
    ) {
        // Compute the alphabet from seen words
        let mut alphabet: AHashMap<char, usize> = AHashMap::new();
        for (word, count) in wc {
            for c in word.chars() {
                *alphabet.entry(c).or_default() += *count as usize;
            }
        }

        // Also include anything from the provided initial alphabet
        for c in &self.initial_alphabet {
            *alphabet.entry(*c).or_default() = usize::MAX;
        }

        let mut kept = alphabet.iter().collect::<Vec<_>>();

        // Compute the number of chars to remove from the alphabet
        // If `limit_alphabet < initial_alphabet.len()`, some of these initial characters
        // will be removed
        let to_remove = self
            .limit_alphabet
            .map(|limit| alphabet.len().saturating_sub(limit))
            .unwrap_or(0);

        // Remove the unwanted chars
        if to_remove > 0 {
            kept.sort_unstable_by_key(|k| *k.1);
            kept.drain(..to_remove);
        }

        // Keep the initial alphabet (sorted for determinism)
        kept.sort_unstable_by_key(|k| *k.0 as u32);
        kept.into_iter().for_each(|(c, _)| {
            let s = c.to_string();
            /*
            if !w2id.contains_key(&s) {
                id2w.push(s.clone());
                w2id.insert(s, (id2w.len() - 1) as u32);
            }
            */
            // u64 hash version
            if !w2id.contains_key(&CompactString::from(&s)) {
                id2w.push(CompactString::from(&s));
                w2id.insert(CompactString::from(&s), (id2w.len() - 1) as u32);
            }
        });
    }

    /// Tokenize words and add subwords to the vocabulary when relevant
    fn tokenize_words(
        &self,
        wc: &AHashMap<CompactString, u64>,
        w2id: &mut AHashMap<CompactString, u32>,
        id2w: &mut Vec<CompactString>,
        p: &Option<ProgressBar>,
    ) -> (Vec<Word>, Vec<u64>) {
        let mut words: Vec<Word> = Vec::with_capacity(wc.len());
        let mut counts: Vec<u64> = Vec::with_capacity(wc.len());

        for (word, count) in wc {
            let mut current_word = Word::new();
            counts.push(*count);

            for (is_first, is_last, c) in word.chars().with_first_and_last() {
                let mut s = c.to_string();
                if w2id.contains_key(&CompactString::from(&s)) {
                    // Found the initial char in the authorized alphabet

                    // Add the `continuing_subword_prefix` if relevant
                    if !is_first {
                        if let Some(prefix) = &self.continuing_subword_prefix {
                            s.insert_str(0, prefix);
                        }
                    }
                    // Add the `end_of_word_suffix` if relevant
                    if is_last {
                        if let Some(suffix) = &self.end_of_word_suffix {
                            s.push_str(suffix);
                        }
                    }

                    // Insert the new formed string if necessary
                    if !w2id.contains_key(&CompactString::from(&s)) {
                        id2w.push(CompactString::from(&s));
                        w2id.insert(CompactString::from(&s), (id2w.len() - 1) as u32);
                    }
                    current_word.add(w2id[&CompactString::from(&s)], 1); // We do not care about the len here
                }
            }
            words.push(current_word);

            if let Some(p) = p {
                p.inc(1);
            }
        }

        (words, counts)
    }

    fn count_pairs(
        &self,
        words: &[Word],
        counts: &[u64],
        p: &Option<ProgressBar>,
    ) -> (AHashMap<Pair, i32>, AHashMap<Pair, AHashSet<usize>>) {
        words
            .maybe_par_iter()
            .enumerate()
            .map(|(i, word)| {
                let mut pair_counts = AHashMap::new();
                let mut where_to_update: AHashMap<Pair, AHashSet<usize>> = AHashMap::new();

                for window in word.get_chars().windows(2) {
                    let cur_pair: Pair = (window[0], window[1]);

                    // Initialize pair_counts and where_to_update for this pair if we just saw it
                    // Then update counts
                    *pair_counts.entry(cur_pair).or_default() += counts[i] as i32;
                    where_to_update.entry(cur_pair).or_default().insert(i);
                }

                if let Some(p) = &p {
                    p.inc(1);
                }

                (pair_counts, where_to_update)
            })
            .reduce(
                || (AHashMap::new(), AHashMap::new()),
                |(mut pair_counts, mut where_to_update), (pc, wtu)| {
                    for (k, v) in pc {
                        *pair_counts.entry(k).or_default() += v;
                    }
                    for (k, v) in wtu {
                        where_to_update.entry(k).or_default().extend(v);
                    }
                    (pair_counts, where_to_update)
                },
            )
    }

    pub fn do_train(
        &self,
        word_counts: &AHashMap<CompactString, u64>,
        model: &mut BPE,
    ) -> Result<Vec<AddedToken>> {
        let mut word_to_id: AHashMap<CompactString, u32> = AHashMap::with_capacity(self.vocab_size);
        let mut id_to_word: Vec<CompactString> = Vec::with_capacity(self.vocab_size);
        let max_token_length: usize = self.max_token_length.unwrap_or(usize::MAX);

        let progress = self.setup_progress();

        //
        // 1. Add all special tokens to the vocabulary
        //
        self.add_special_tokens(&mut word_to_id, &mut id_to_word);

        //
        // 2. Compute the initial alphabet
        //
        self.compute_alphabet(word_counts, &mut word_to_id, &mut id_to_word);

        //
        // 3. Tokenize words
        //
        self.update_progress(&progress, word_counts.len(), "Tokenize words");
        let (mut words, counts) =
            self.tokenize_words(word_counts, &mut word_to_id, &mut id_to_word, &progress);
        self.finalize_progress(&progress, words.len());

        //
        // 4. Count pairs in words
        //
        self.update_progress(&progress, words.len(), "Count pairs");
        let (mut pair_counts, mut where_to_update) = self.count_pairs(&words, &counts, &progress);
        // Insert them in the queue
        let mut queue = OctonaryHeap::with_capacity(pair_counts.len());
        where_to_update.drain().for_each(|(pair, pos)| {
            let count = pair_counts[&pair];
            if count > 0 {
                queue.push(Merge {
                    pair,
                    count: count as u64,
                    pos,
                });
            }
        });
        self.finalize_progress(&progress, words.len());

        //
        // 5. Do merges
        //
        self.update_progress(&progress, self.vocab_size, "Compute merges");
        let mut merges: Vec<(Pair, u32)> = vec![];
        loop {
            // Stop as soon as we have a big enough vocabulary
            if word_to_id.len() >= self.vocab_size {
                break;
            }

            let Some(mut top) = queue.pop() else {
                break;
            };

            if top.count != pair_counts[&top.pair] as u64 {
                top.count = pair_counts[&top.pair] as u64;
                queue.push(top);
                continue;
            }

            if top.count < 1 || self.min_frequency > top.count {
                break;
            }

            let part_a = &id_to_word[top.pair.0 as usize];
            let mut part_b = id_to_word[top.pair.1 as usize].as_str();

            // Build new token
            if let Some(prefix) = &self.continuing_subword_prefix {
                if let Some(rest) = part_b.strip_prefix(prefix) {
                    part_b = rest;
                }
            }
            let new_token = format!("{part_a}{part_b}");
            // implement sentencepiece-like merge.
            // if this code were to be merged, integrate a way in the python bindings to communicate this variable
            // default should be 0/None to maintain previous behavior. 16 is the spm default.

            // Insert new token if it does not already exist
            let new_token_id = word_to_id
                .get(&CompactString::from(&new_token))
                .copied()
                .unwrap_or(id_to_word.len() as u32);
            if !word_to_id.contains_key(&CompactString::from(&new_token)) {
                id_to_word.push(CompactString::from(&new_token));
                word_to_id.insert(CompactString::from(&new_token), new_token_id);
            }
            merges.push((top.pair, new_token_id));

            // Merge the new pair in every words
            // Safety: This is just a type assertion, the code below may no longer be safe
            // if the type of `pos` changes
            let pos: &AHashSet<usize> = &top.pos;

            let words_len = words.len();
            struct WordPtr(*mut Word);
            // Safety: We do not actually use this for concurrent access to the same memory,
            // only to different chunks within the same allocation.
            unsafe impl Sync for WordPtr {}
            let word_start = WordPtr(words.as_mut_ptr());

            let changes = pos
                .maybe_par_iter()
                .flat_map(|&i| {
                    // We can merge each of these words in parallel here because each position
                    // can be there only once (AHashSet). So this is safe.
                    unsafe {
                        assert!(i < words_len);
                        // This is words[i], but avoids needing to go through &T (which triggers UB)
                        let word = word_start.0.add(i);
                        // let word: &mut Word = &mut (*word);
                        (*word)
                            .merge(top.pair.0, top.pair.1, new_token_id, max_token_length)
                            .into_iter()
                            .map(|c| (c, i))
                            .collect::<Vec<_>>()
                    }
                })
                .collect::<Vec<_>>();

            // Introduce new formed pairs
            for ((pair, change), iw) in changes {
                let count = change * counts[iw] as i32;
                *pair_counts.entry(pair).or_default() += count;
                if change > 0 {
                    where_to_update.entry(pair).or_default().insert(iw);
                }
            }
            where_to_update.drain().for_each(|(pair, pos)| {
                let count = pair_counts[&pair];
                if count > 0 {
                    queue.push(Merge {
                        pair,
                        count: count as u64,
                        pos,
                    });
                }
            });

            if let Some(p) = &progress {
                p.inc(1);
            }
        }
        self.finalize_progress(&progress, merges.len());

        // Transfer new vocab & options to model
        //model.vocab = word_to_id;
        model.vocab = word_to_id
            .into_iter()
            // we have to look up the string in id_to_word because the key in word_to_id is a hash
            .map(|(_key, val)| (id_to_word[val as usize].to_string(), val))
            .collect();
        model.vocab_r = model
            .vocab
            .iter()
            .map(|(key, val)| (*val, key.to_owned()))
            .collect();
        model.merges = merges
            .into_iter()
            .enumerate()
            .map(|(i, (pair, new_token_id))| (pair, (i as u32, new_token_id)))
            .collect();

        model.continuing_subword_prefix = self.continuing_subword_prefix.clone();
        model.end_of_word_suffix = self.end_of_word_suffix.clone();

        Ok(self.special_tokens.clone())
    }
}

impl Trainer for BpeTrainer {
    type Model = BPE;

    /// Train a BPE model
    fn train(&self, model: &mut BPE) -> Result<Vec<AddedToken>> {
        self.do_train(&self.words, model)
    }

    /// Whether we should show progress
    fn should_show_progress(&self) -> bool {
        self.show_progress
    }

    fn feed<I, S, F>(&mut self, iterator: I, process: F) -> Result<()>
    where
        I: Iterator<Item = S> + Send,
        S: AsRef<str> + Send,
        F: Fn(&str) -> Result<Vec<String>> + Sync,
    {
        let words: Result<AHashMap<CompactString, u64>> = iterator
            .maybe_par_bridge()
            .map(|sequence| {
                let words = process(sequence.as_ref())?;
                let mut map = AHashMap::new();
                for word in words {
                    *map.entry(CompactString::from(word)).or_default() += 1;
                }
                Ok(map)
            })
            .reduce(
                || Ok(AHashMap::new()),
                |acc, ws| {
                    let mut acc = acc?;
                    for (k, v) in ws? {
                        *acc.entry(k).or_default() += v;
                    }
                    Ok(acc)
                },
            );

        self.words = words?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{BpeTrainer, Pair, BPE};
    use ahash::AHashMap;
    use compact_str::CompactString;

    #[test]
    fn test_train() {
        let word_counts: AHashMap<CompactString, u64> = [
            ("roses".into(), 1),
            ("are".into(), 2),
            ("red".into(), 1),
            ("voilets".into(), 1),
            ("blue".into(), 1),
            ("BERT".into(), 1),
            ("is".into(), 2),
            ("big".into(), 1),
            ("and".into(), 1),
            ("so".into(), 1),
            ("GPT-2".into(), 1),
        ]
        .iter()
        .cloned()
        .collect();
        let trainer = BpeTrainer::builder()
            .show_progress(false)
            .min_frequency(2)
            .build();
        let mut model = BPE::default();
        trainer.do_train(&word_counts, &mut model).unwrap();

        // Vocab should contain all of the characters from the `word_counts` mapping
        // as well as three merges: 're', 'are', and 'is'.
        let expected_vocab: AHashMap<String, u32> = [
            ("-".into(), 0),
            ("2".into(), 1),
            ("B".into(), 2),
            ("E".into(), 3),
            ("G".into(), 4),
            ("P".into(), 5),
            ("R".into(), 6),
            ("T".into(), 7),
            ("a".into(), 8),
            ("b".into(), 9),
            ("d".into(), 10),
            ("e".into(), 11),
            ("g".into(), 12),
            ("i".into(), 13),
            ("l".into(), 14),
            ("n".into(), 15),
            ("o".into(), 16),
            ("r".into(), 17),
            ("s".into(), 18),
            ("t".into(), 19),
            ("u".into(), 20),
            ("v".into(), 21),
            ("re".into(), 22),
            ("are".into(), 23),
            ("is".into(), 24),
        ]
        .iter()
        .cloned()
        .collect();
        assert_eq!(model.vocab, expected_vocab);

        // The keys in `merges` are pairs of symbols, the values are tuples of (rank, id),
        // where 'rank' determines the order in which this merge will be applied during
        // tokenization, and 'id' is the vocab id of the symbol resulting from merging
        // the pair of symbols in the corresponding key.
        let expected_merges: AHashMap<Pair, (u32, u32)> = [
            ((17, 11), (0, 22)), // 'r' + 'e'  -> 're'
            ((8, 22), (1, 23)),  // 'a' + 're' -> 'are'
            ((13, 18), (2, 24)), // 'i' + 's'  -> 'is'
        ]
        .iter()
        .cloned()
        .collect();
        assert_eq!(model.merges, expected_merges);
    }
    #[test]
    fn bpe_test_max_token_length_16() {
        /* bpe_test_max_token_length series of tests test the max_token_length flag of bpetrainer
        // this is the more robust version that only tests max length of learned tokens
        // (pre) tokenizer settings or vocab can be easily modified when necessary
         */

        let max_token_length = 16;
        let long_word_counts: AHashMap<CompactString, u64> = [
            ("singlelongtokenwithoutcasechange", 2),
            ("singleLongTokenWithCamelCaseChange", 2),
            ("Longsingletokenwithpunctu@t!onwithin", 2),
            ("Anotherlongsingletokenwithnumberw1th1n", 2),
            ("짧은한글문자열짧은한", 2),             // korean 10 char
            ("긴한글문자열긴한글문자열긴한글문", 2), // korean 16 char
            ("短字符串短字符串短字", 2),             //simplified chinese 10 char
            ("长字符串长字符串长字符串长字符串", 2), // simp. chinese 16 char
            ("短い文字列短い文字列", 2),             // japanese 10 char
            ("長い文字列長い文字列長い文字列長", 2), // japanese 16 char
            ("so", 2),
            ("GPT-2", 2),
        ]
        .iter()
        .map(|(key, value)| (CompactString::from(key.to_string()), *value))
        .collect();
        let trainer = BpeTrainer::builder()
            .max_token_length(Some(max_token_length))
            .show_progress(false)
            .min_frequency(0)
            .build();
        let mut model = BPE::default();
        trainer.do_train(&long_word_counts, &mut model).unwrap();
        let vocab = model.get_vocab();
        for token in vocab.keys() {
            assert!(
                token.chars().count() <= max_token_length,
                "token too long : {} , chars().count() = {}",
                token,
                token.chars().count()
            )
        }
    }
    #[test]
    fn bpe_test_max_token_length_direct_assert() {
        /* more direct version of bpe_test_max_token_length test
        // directly compares tokens with known expected values.
        // maybe unstable depending on specific settings or changes.
         */
        let long_word_counts: AHashMap<CompactString, u64> = [
            ("sin", 2),
            ("Sin", 2),
            ("Lon", 2),
            ("Ano", 2),
            ("짧은한", 2),
            ("긴한글", 2),
            ("短字符", 2),
            ("长字符", 2),
            ("短い文", 2),
            ("長い文", 2),
            ("so", 2),
            ("GP", 2),
        ]
        .iter()
        .map(|(key, value)| (CompactString::from(key.to_string()), *value))
        .collect();
        let trainer = BpeTrainer::builder()
            .max_token_length(Some(2))
            .show_progress(false)
            .min_frequency(0)
            .build();
        let mut model = BPE::default();
        trainer.do_train(&long_word_counts, &mut model).unwrap();
        let trained_vocab: AHashMap<String, u32> = model.get_vocab().into_iter().collect();
        let expected_vocab: AHashMap<String, u32> = [
            ("短", 12),
            ("n", 6),
            ("i", 5),
            ("s", 8),
            ("字符", 23),
            ("長", 14),
            ("긴", 17),
            ("い文", 22),
            ("L", 2),
            ("in", 21),
            ("o", 7),
            ("은한", 29),
            ("S", 4),
            ("P", 3),
            ("so", 27),
            ("符", 13),
            ("文", 11),
            ("字", 10),
            ("짧", 19),
            ("GP", 25),
            ("글", 16),
            ("G", 1),
            ("An", 24),
            ("长", 15),
            ("A", 0),
            ("Lo", 26),
            ("긴한", 28),
            ("い", 9),
            ("한", 20),
            ("은", 18),
        ]
        .iter()
        .cloned()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        assert_eq!(trained_vocab, expected_vocab)
    }
}

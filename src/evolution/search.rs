//! # Stage: EvolutionarySearch (Task 4.2 -- Evolutionary Parameter Search)
//!
//! ## Responsibility
//! Use genetic algorithm techniques to search the parameter space for optimal
//! configurations. Maintains a population of parameter sets, evaluates fitness,
//! and evolves via crossover and mutation.
//!
//! ## Guarantees
//! - Thread-safe: all operations use `Arc<Mutex<Inner>>`
//! - Bounded: population size and generation count are configurable limits
//! - Deterministic: given the same seed and sequence of operations, results are reproducible
//! - Non-panicking: every public method returns `Result`
//!
//! ## NOT Responsible For
//! - Applying configurations (passes results to controller)
//! - Running experiments (fitness is reported externally)

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors specific to evolutionary search.
#[derive(Debug, Error)]
pub enum SearchError {
    /// Internal lock was poisoned by a panicking thread.
    #[error("evolutionary search lock poisoned")]
    LockPoisoned,

    /// Attempted to evolve with an empty population.
    #[error("cannot evolve an empty population")]
    EmptyPopulation,

    /// Not enough generations have elapsed.
    #[error("insufficient generations: have {have}, need {need}")]
    InsufficientGenerations {
        /// Number of generations completed.
        have: usize,
        /// Number of generations required.
        need: usize,
    },

    /// A referenced individual was not found.
    #[error("individual not found: {0}")]
    IndividualNotFound(String),
}

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Configuration for the evolutionary search algorithm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvolutionConfig {
    /// Number of individuals in the population.
    pub population_size: usize,
    /// Number of top individuals preserved across generations (elitism).
    pub elite_count: usize,
    /// Probability of mutation per gene (0.0 to 1.0).
    pub mutation_rate: f64,
    /// Strength of mutation noise relative to parameter range.
    pub mutation_strength: f64,
    /// Probability of crossover between two parents (0.0 to 1.0).
    pub crossover_rate: f64,
    /// Maximum number of generations before stopping.
    pub max_generations: usize,
    /// Bounds for each parameter: name -> (min, max).
    pub parameter_bounds: HashMap<String, (f64, f64)>,
}

impl Default for EvolutionConfig {
    fn default() -> Self {
        Self {
            population_size: 20,
            elite_count: 2,
            mutation_rate: 0.1,
            mutation_strength: 0.05,
            crossover_rate: 0.7,
            max_generations: 100,
            parameter_bounds: HashMap::new(),
        }
    }
}

/// A single individual in the population (a candidate parameter set).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Individual {
    /// Parameter values (gene name -> value).
    pub genes: HashMap<String, f64>,
    /// Fitness score (higher is better).
    pub fitness: f64,
    /// Generation this individual was created in.
    pub generation: usize,
    /// Unique identifier for this individual.
    pub id: String,
}

/// Result of a single generation of evolution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    /// Best individual found in this generation.
    pub best: Individual,
    /// Generation number.
    pub generation: usize,
    /// Average fitness of the population.
    pub population_fitness_avg: f64,
    /// Best fitness in the population.
    pub population_fitness_best: f64,
    /// Whether the search has converged.
    pub converged: bool,
}

// ---------------------------------------------------------------------------
// Inner state
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct Inner {
    config: EvolutionConfig,
    population: Vec<Individual>,
    generation: usize,
    best_ever: Option<Individual>,
    history: Vec<SearchResult>,
    rng_state: u64,
}

impl Inner {
    fn next_rng(&mut self) -> u64 {
        // xorshift64 — lightweight, deterministic PRNG
        let mut x = self.rng_state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng_state = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_rng() % 1_000_000) as f64 / 1_000_000.0
    }
}

// ---------------------------------------------------------------------------
// EvolutionarySearch
// ---------------------------------------------------------------------------

/// Genetic algorithm search engine for parameter optimisation.
///
/// Cheap to clone -- all clones share the same inner state via `Arc<Mutex<_>>`.
#[derive(Debug, Clone)]
pub struct EvolutionarySearch {
    inner: Arc<Mutex<Inner>>,
}

impl EvolutionarySearch {
    /// Create a new `EvolutionarySearch` with the given configuration.
    ///
    /// # Panics
    /// This function never panics.
    pub fn new(config: EvolutionConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                config,
                population: Vec::new(),
                generation: 0,
                best_ever: None,
                history: Vec::new(),
                rng_state: 42,
            })),
        }
    }

    /// Create with a specific RNG seed for reproducibility.
    ///
    /// # Panics
    /// This function never panics.
    pub fn with_seed(config: EvolutionConfig, seed: u64) -> Self {
        let seed = if seed == 0 { 1 } else { seed };
        Self {
            inner: Arc::new(Mutex::new(Inner {
                config,
                population: Vec::new(),
                generation: 0,
                best_ever: None,
                history: Vec::new(),
                rng_state: seed,
            })),
        }
    }

    /// Randomly initialise the population within configured parameter bounds.
    ///
    /// # Errors
    /// Returns [`SearchError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn initialize_population(&self) -> Result<(), SearchError> {
        let mut inner = self.inner.lock().map_err(|_| SearchError::LockPoisoned)?;
        let size = inner.config.population_size;
        let bounds = inner.config.parameter_bounds.clone();

        inner.population.clear();
        for i in 0..size {
            let mut genes = HashMap::new();
            for (name, (min, max)) in &bounds {
                let t = inner.next_f64();
                let value = min + t * (max - min);
                genes.insert(name.clone(), value);
            }
            inner.population.push(Individual {
                genes,
                fitness: 0.0,
                generation: 0,
                id: format!("ind-gen0-{i}"),
            });
        }

        Ok(())
    }

    /// Set the fitness score for an individual by ID.
    ///
    /// # Errors
    /// - [`SearchError::IndividualNotFound`] if the ID is unknown.
    /// - [`SearchError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn evaluate(&self, id: &str, fitness: f64) -> Result<(), SearchError> {
        let mut inner = self.inner.lock().map_err(|_| SearchError::LockPoisoned)?;
        // Determine whether the new fitness would beat best_ever before mutably borrowing population.
        let dominated = match &inner.best_ever {
            Some(best) => fitness > best.fitness,
            None => true,
        };

        let individual = inner
            .population
            .iter_mut()
            .find(|ind| ind.id == id)
            .ok_or_else(|| SearchError::IndividualNotFound(id.to_string()))?;
        individual.fitness = fitness;

        // Update best_ever
        if dominated {
            inner.best_ever = Some(individual.clone());
        }

        Ok(())
    }

    /// Run one generation of evolution: selection, crossover, mutation.
    ///
    /// Returns a [`SearchResult`] summarising the generation.
    ///
    /// # Errors
    /// - [`SearchError::EmptyPopulation`] if the population is empty.
    /// - [`SearchError::LockPoisoned`] if the internal lock is poisoned.
    ///
    /// # Panics
    /// This function never panics.
    pub fn evolve(&self) -> Result<SearchResult, SearchError> {
        let mut inner = self.inner.lock().map_err(|_| SearchError::LockPoisoned)?;

        if inner.population.is_empty() {
            return Err(SearchError::EmptyPopulation);
        }

        inner.generation += 1;
        let gen = inner.generation;

        // Sort by fitness descending
        inner.population.sort_by(|a, b| {
            b.fitness
                .partial_cmp(&a.fitness)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Compute stats
        let best_fitness = inner.population[0].fitness;
        let avg_fitness: f64 =
            inner.population.iter().map(|i| i.fitness).sum::<f64>() / inner.population.len() as f64;

        // Preserve elites
        let elite_count = inner.config.elite_count.min(inner.population.len());
        let mut new_pop: Vec<Individual> = inner.population[..elite_count]
            .iter()
            .enumerate()
            .map(|(i, elite)| Individual {
                genes: elite.genes.clone(),
                fitness: elite.fitness,
                generation: gen,
                id: format!("ind-gen{gen}-{i}"),
            })
            .collect();

        let target_size = inner.config.population_size;
        let crossover_rate = inner.config.crossover_rate;
        let mutation_rate = inner.config.mutation_rate;
        let mutation_strength = inner.config.mutation_strength;
        let bounds = inner.config.parameter_bounds.clone();
        let pop_len = inner.population.len();

        // Clone population snapshot so we can freely mutate rng_state in the loop.
        let pop_snapshot: Vec<Individual> = inner.population.clone();
        let rng = &mut inner.rng_state;

        // Fill rest with offspring
        let mut child_idx = elite_count;
        while new_pop.len() < target_size {
            // Tournament selection: pick 3, take best
            let parent1_genes = tournament_select(&pop_snapshot, pop_len, rng).genes.clone();
            let parent2_genes = tournament_select(&pop_snapshot, pop_len, rng).genes.clone();

            // Crossover
            let cross_roll = (xorshift_mod(rng, 1_000_000) as f64) / 1_000_000.0;
            let mut child_genes = if cross_roll < crossover_rate {
                uniform_crossover(&parent1_genes, &parent2_genes, rng)
            } else {
                parent1_genes.clone()
            };

            // Mutation
            for (name, value) in &mut child_genes {
                let mut_roll = (xorshift_mod(rng, 1_000_000) as f64) / 1_000_000.0;
                if mut_roll < mutation_rate {
                    if let Some(&(min, max)) = bounds.get(name) {
                        let range = max - min;
                        let noise_roll = (xorshift_mod(rng, 1_000_000) as f64) / 1_000_000.0;
                        let noise = (noise_roll - 0.5) * 2.0 * mutation_strength * range;
                        *value = (*value + noise).clamp(min, max);
                    }
                }
            }

            new_pop.push(Individual {
                genes: child_genes,
                fitness: 0.0,
                generation: gen,
                id: format!("ind-gen{gen}-{child_idx}"),
            });
            child_idx += 1;
        }

        inner.population = new_pop;

        // Check convergence
        let converged = check_convergence(&inner.population);

        // Update best_ever
        if let Some(current_best) = inner.population.first() {
            let dominated = match &inner.best_ever {
                Some(best) => current_best.fitness > best.fitness,
                None => true,
            };
            if dominated {
                inner.best_ever = Some(current_best.clone());
            }
        }

        let result = SearchResult {
            best: inner
                .best_ever
                .clone()
                .unwrap_or_else(|| inner.population[0].clone()),
            generation: gen,
            population_fitness_avg: avg_fitness,
            population_fitness_best: best_fitness,
            converged,
        };

        inner.history.push(result.clone());
        Ok(result)
    }

    /// Return the best individual seen across all generations.
    ///
    /// # Panics
    /// This function never panics.
    pub fn best_individual(&self) -> Option<Individual> {
        match self.inner.lock() {
            Ok(g) => g.best_ever.clone(),
            Err(_) => None,
        }
    }

    /// Return the current generation number.
    ///
    /// # Panics
    /// This function never panics.
    pub fn current_generation(&self) -> usize {
        match self.inner.lock() {
            Ok(g) => g.generation,
            Err(_) => 0,
        }
    }

    /// Return a snapshot of the current population.
    ///
    /// # Panics
    /// This function never panics.
    pub fn population(&self) -> Vec<Individual> {
        match self.inner.lock() {
            Ok(g) => g.population.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// Return the most recent search result, if any.
    ///
    /// # Panics
    /// This function never panics.
    pub fn search_result(&self) -> Option<SearchResult> {
        match self.inner.lock() {
            Ok(g) => g.history.last().cloned(),
            Err(_) => None,
        }
    }

    /// Check if the population has converged (top 5 individuals within 1% of each other).
    ///
    /// # Panics
    /// This function never panics.
    pub fn is_converged(&self) -> bool {
        match self.inner.lock() {
            Ok(g) => check_convergence(&g.population),
            Err(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn tournament_select<'a>(
    population: &'a [Individual],
    pop_len: usize,
    rng_state: &mut u64,
) -> &'a Individual {
    let mut best_idx = xorshift_mod(rng_state, pop_len);
    for _ in 0..2 {
        let candidate = xorshift_mod(rng_state, pop_len);
        if population[candidate].fitness > population[best_idx].fitness {
            best_idx = candidate;
        }
    }
    &population[best_idx]
}

fn uniform_crossover(
    parent1: &HashMap<String, f64>,
    parent2: &HashMap<String, f64>,
    rng_state: &mut u64,
) -> HashMap<String, f64> {
    let mut child = HashMap::new();
    for (key, &v1) in parent1 {
        let v2 = parent2.get(key).copied().unwrap_or(v1);
        let pick = xorshift_mod(rng_state, 2);
        child.insert(key.clone(), if pick == 0 { v1 } else { v2 });
    }
    child
}

fn check_convergence(population: &[Individual]) -> bool {
    if population.len() < 5 {
        return false;
    }
    let mut sorted: Vec<f64> = population.iter().map(|i| i.fitness).collect();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let top5 = &sorted[..5];
    let best = top5[0];
    if best.abs() < f64::EPSILON {
        return top5.iter().all(|&f| f.abs() < f64::EPSILON);
    }
    top5.iter().all(|&f| ((f - best) / best).abs() < 0.01)
}

fn xorshift_mod(state: &mut u64, modulus: usize) -> usize {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    (x as usize) % modulus
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> EvolutionConfig {
        let mut bounds = HashMap::new();
        bounds.insert("learning_rate".to_string(), (0.001, 0.1));
        bounds.insert("batch_size".to_string(), (8.0, 128.0));
        bounds.insert("dropout".to_string(), (0.0, 0.5));
        EvolutionConfig {
            population_size: 10,
            elite_count: 2,
            mutation_rate: 0.2,
            mutation_strength: 0.1,
            crossover_rate: 0.7,
            max_generations: 50,
            parameter_bounds: bounds,
        }
    }

    #[test]
    fn test_initialize_population_creates_correct_size() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        let pop = search.population();
        assert_eq!(pop.len(), 10);
    }

    #[test]
    fn test_initialize_population_within_bounds() {
        let config = test_config();
        let bounds = config.parameter_bounds.clone();
        let search = EvolutionarySearch::new(config);
        search.initialize_population().unwrap();
        for ind in search.population() {
            for (name, &value) in &ind.genes {
                let (min, max) = bounds[name];
                assert!(
                    value >= min && value <= max,
                    "{name}={value} outside [{min}, {max}]"
                );
            }
        }
    }

    #[test]
    fn test_evaluate_updates_fitness() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        let pop = search.population();
        let id = &pop[0].id;
        search.evaluate(id, 42.0).unwrap();
        let updated = search.population();
        let ind = updated.iter().find(|i| i.id == *id).unwrap();
        assert!((ind.fitness - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_evaluate_individual_not_found() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        let err = search.evaluate("nonexistent", 1.0).unwrap_err();
        assert!(matches!(err, SearchError::IndividualNotFound(_)));
    }

    #[test]
    fn test_evolve_advances_generation() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        // Assign some fitness
        for ind in search.population() {
            search.evaluate(&ind.id, ind.genes.values().sum()).unwrap();
        }
        let result = search.evolve().unwrap();
        assert_eq!(result.generation, 1);
        assert_eq!(search.current_generation(), 1);
    }

    #[test]
    fn test_evolve_empty_population_error() {
        let search = EvolutionarySearch::new(test_config());
        let err = search.evolve().unwrap_err();
        assert!(matches!(err, SearchError::EmptyPopulation));
    }

    #[test]
    fn test_elite_preserved() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        // Give the first two high fitness
        let pop = search.population();
        search.evaluate(&pop[0].id, 100.0).unwrap();
        search.evaluate(&pop[1].id, 90.0).unwrap();
        for ind in pop.iter().skip(2) {
            search.evaluate(&ind.id, 1.0).unwrap();
        }
        let result = search.evolve().unwrap();
        assert!(result.population_fitness_best >= 90.0);
    }

    #[test]
    fn test_best_individual_tracks_across_generations() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        let pop = search.population();
        search.evaluate(&pop[0].id, 50.0).unwrap();
        for ind in pop.iter().skip(1) {
            search.evaluate(&ind.id, 1.0).unwrap();
        }
        search.evolve().unwrap();
        let best = search.best_individual().unwrap();
        assert!(best.fitness >= 50.0);
    }

    #[test]
    fn test_convergence_detection_not_converged() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        // Spread fitness widely
        for (i, ind) in search.population().iter().enumerate() {
            search.evaluate(&ind.id, (i as f64) * 10.0).unwrap();
        }
        assert!(!search.is_converged());
    }

    #[test]
    fn test_convergence_detection_converged() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        // Set all to same fitness
        for ind in search.population() {
            search.evaluate(&ind.id, 100.0).unwrap();
        }
        assert!(search.is_converged());
    }

    #[test]
    fn test_population_size_maintained_after_evolve() {
        let config = test_config();
        let expected = config.population_size;
        let search = EvolutionarySearch::new(config);
        search.initialize_population().unwrap();
        for ind in search.population() {
            search.evaluate(&ind.id, 10.0).unwrap();
        }
        search.evolve().unwrap();
        assert_eq!(search.population().len(), expected);
    }

    #[test]
    fn test_config_defaults() {
        let config = EvolutionConfig::default();
        assert_eq!(config.population_size, 20);
        assert_eq!(config.elite_count, 2);
        assert!((config.mutation_rate - 0.1).abs() < f64::EPSILON);
        assert_eq!(config.max_generations, 100);
    }

    #[test]
    fn test_serialization_config() {
        let config = test_config();
        let json = serde_json::to_string(&config).unwrap();
        let back: EvolutionConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.population_size, config.population_size);
    }

    #[test]
    fn test_serialization_individual() {
        let mut genes = HashMap::new();
        genes.insert("lr".to_string(), 0.01);
        let ind = Individual {
            genes,
            fitness: 42.0,
            generation: 3,
            id: "ind-1".to_string(),
        };
        let json = serde_json::to_string(&ind).unwrap();
        let back: Individual = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "ind-1");
        assert!((back.fitness - 42.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_clone_shares_state() {
        let search = EvolutionarySearch::new(test_config());
        let search2 = search.clone();
        search.initialize_population().unwrap();
        assert_eq!(search2.population().len(), 10);
    }

    #[test]
    fn test_search_result_after_evolve() {
        let search = EvolutionarySearch::new(test_config());
        search.initialize_population().unwrap();
        for ind in search.population() {
            search.evaluate(&ind.id, 5.0).unwrap();
        }
        search.evolve().unwrap();
        let result = search.search_result().unwrap();
        assert_eq!(result.generation, 1);
    }

    #[test]
    fn test_with_seed_reproducibility() {
        let config = test_config();
        let s1 = EvolutionarySearch::with_seed(config.clone(), 123);
        let s2 = EvolutionarySearch::with_seed(config, 123);
        s1.initialize_population().unwrap();
        s2.initialize_population().unwrap();
        let pop1 = s1.population();
        let pop2 = s2.population();
        for (a, b) in pop1.iter().zip(pop2.iter()) {
            for (key, &val) in &a.genes {
                let other = b.genes[key];
                assert!((val - other).abs() < f64::EPSILON, "seed mismatch on {key}");
            }
        }
    }

    #[test]
    fn test_mutation_stays_within_bounds() {
        let config = test_config();
        let bounds = config.parameter_bounds.clone();
        let search = EvolutionarySearch::new(config);
        search.initialize_population().unwrap();
        for ind in search.population() {
            search.evaluate(&ind.id, 10.0).unwrap();
        }
        // Run several generations
        for _ in 0..5 {
            search.evolve().unwrap();
            for ind in search.population() {
                search.evaluate(&ind.id, ind.genes.values().sum()).unwrap();
            }
        }
        for ind in search.population() {
            for (name, &value) in &ind.genes {
                if let Some(&(min, max)) = bounds.get(name) {
                    assert!(
                        value >= min && value <= max,
                        "gen {} {name}={value} outside [{min}, {max}]",
                        ind.generation
                    );
                }
            }
        }
    }

    #[test]
    fn test_error_display() {
        let err = SearchError::EmptyPopulation;
        assert!(err.to_string().contains("empty population"));

        let err = SearchError::IndividualNotFound("x".to_string());
        assert!(err.to_string().contains("x"));

        let err = SearchError::InsufficientGenerations { have: 1, need: 5 };
        assert!(err.to_string().contains("1"));
    }
}

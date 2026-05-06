//! Glicko-2 rating system, full periodized implementation.
//!
//! Source: Glickman 2013, "Example of the Glicko-2 system"
//! https://glicko.net/glicko/glicko2.pdf
//!
//! Key idea (and the part the previous per-match implementation got wrong):
//! Glicko-2 batches all of a player's results within a *rating period* and
//! applies a single update at the end of the period. Per-match updates
//! systematically mis-state the volatility and RD trajectories — players who
//! play 10 matches in a session get their σ updated 10× as fast as Glickman's
//! math expects, which both inflates rating moves on streaks and underweights
//! consistent performance.
//!
//! In addition, players who do NOT play in a period still need their phi
//! (rating deviation) inflated according to:
//!     phi' = sqrt(phi^2 + sigma^2)
//! This represents the growing uncertainty in their skill estimate over time.
//!
//! This module implements both halves: `update_player_with_results` for
//! active players, and `decay_inactive_player` for idle ones.

use serde::{Deserialize, Serialize};

/// Glickman's "constant scaling factor" between Glicko (mu=1500, phi=350)
/// and Glicko-2 (mu=0, phi=350/SCALE) coordinate systems.
const SCALE: f64 = 173.7178;

pub const DEFAULT_MU: f64 = 1500.0;
pub const DEFAULT_PHI: f64 = 350.0;
pub const DEFAULT_SIGMA: f64 = 0.06;

/// "Reasonable choices [for tau] are between 0.3 and 1.2." (Glickman §4)
/// Lower tau = ratings more stable, higher = more responsive to streaks.
/// 0.5 balances both for a casual ladder.
const TAU: f64 = 0.5;

/// Convergence tolerance for the volatility solver.
const EPSILON: f64 = 1e-6;

/// One opponent encounter within a rating period.
/// `opponent_*` are the opponent's *pre-period* ratings.
/// `score` is 1.0 (win), 0.0 (loss), or 0.5 (draw).
#[derive(Debug, Clone, Copy)]
pub struct PeriodResult {
    pub opponent_mu: f64,
    pub opponent_phi: f64,
    pub score: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rating {
    pub mu: f64,
    pub phi: f64,
    pub sigma: f64,
}

impl Default for Rating {
    fn default() -> Self {
        Self { mu: DEFAULT_MU, phi: DEFAULT_PHI, sigma: DEFAULT_SIGMA }
    }
}

/// Apply the periodized Glicko-2 update to a player given all their results
/// in a rating period. Modifies the player's mu, phi, sigma in place.
///
/// Implements steps 2-7 from Glickman §3.1 verbatim. If the player had no
/// results this period, call `decay_inactive_player` instead.
pub fn update_player_with_results(player: &mut Rating, results: &[PeriodResult]) {
    if results.is_empty() {
        decay_inactive_player(player);
        return;
    }

    // Step 2: convert player's rating to the Glicko-2 scale.
    let mu = (player.mu - 1500.0) / SCALE;
    let phi = player.phi / SCALE;
    let sigma = player.sigma;

    // Step 3: compute v (estimated variance based on game outcomes).
    // v = ( Σ g(phi_j)^2 * E(mu, mu_j, phi_j) * (1 - E(mu, mu_j, phi_j)) )^-1
    let mut v_inv = 0.0;
    for r in results {
        let mu_j = (r.opponent_mu - 1500.0) / SCALE;
        let phi_j = r.opponent_phi / SCALE;
        let g_j = g(phi_j);
        let e_j = e(mu, mu_j, g_j);
        v_inv += g_j.powi(2) * e_j * (1.0 - e_j);
    }
    let v = 1.0 / v_inv;

    // Step 4: compute Δ (estimated improvement in rating).
    // Δ = v * Σ g(phi_j) * (s_j - E(mu, mu_j, phi_j))
    let mut delta_sum = 0.0;
    for r in results {
        let mu_j = (r.opponent_mu - 1500.0) / SCALE;
        let phi_j = r.opponent_phi / SCALE;
        let g_j = g(phi_j);
        let e_j = e(mu, mu_j, g_j);
        delta_sum += g_j * (r.score - e_j);
    }
    let delta = v * delta_sum;

    // Step 5: compute new volatility σ' via the iterative algorithm in §3.2.
    let sigma_new = compute_new_volatility(delta, phi, v, sigma);

    // Step 6: update RD.
    let phi_star = (phi.powi(2) + sigma_new.powi(2)).sqrt();
    let phi_new = 1.0 / (1.0 / phi_star.powi(2) + 1.0 / v).sqrt();

    // Step 7: update rating.
    let mu_new = mu + phi_new.powi(2) * delta_sum;

    // Step 8: convert back to the original Glicko-1 scale.
    player.mu = mu_new * SCALE + 1500.0;
    player.phi = phi_new * SCALE;
    player.sigma = sigma_new;
}

/// Inactive-player update for one rating period. Inflates phi to reflect
/// the increased uncertainty about an idle player's true skill, but leaves
/// mu and sigma untouched. From Glickman §3.1 step 6 ("If the player did
/// not compete during the rating period, only Step 6 is carried out").
pub fn decay_inactive_player(player: &mut Rating) {
    let phi = player.phi / SCALE;
    let phi_star = (phi.powi(2) + player.sigma.powi(2)).sqrt();
    player.phi = phi_star * SCALE;
}

fn g(phi: f64) -> f64 {
    1.0 / (1.0 + 3.0 * phi.powi(2) / std::f64::consts::PI.powi(2)).sqrt()
}

fn e(mu: f64, mu_opp: f64, g_opp: f64) -> f64 {
    1.0 / (1.0 + (-g_opp * (mu - mu_opp)).exp())
}

/// Iterative algorithm from Glickman §3.2 for computing the new volatility.
/// Uses the Illinois method on f(x) = (eq. defined in the paper). Bounded
/// by 100 iterations; in practice converges in 5-10.
fn compute_new_volatility(delta: f64, phi: f64, v: f64, sigma: f64) -> f64 {
    let a = (sigma.powi(2)).ln();
    let phi2 = phi.powi(2);
    let delta2 = delta.powi(2);

    let f = |x: f64| -> f64 {
        let exp_x = x.exp();
        let denom = phi2 + v + exp_x;
        exp_x * (delta2 - phi2 - v - exp_x) / (2.0 * denom.powi(2))
            - (x - a) / TAU.powi(2)
    };

    // Choose initial bracket [A, B] containing a root of f.
    let mut big_a = a;
    let mut big_b = if delta2 > phi2 + v {
        (delta2 - phi2 - v).ln()
    } else {
        let mut k = 1.0;
        while f(a - k * TAU) < 0.0 {
            k += 1.0;
        }
        a - k * TAU
    };

    let mut f_a = f(big_a);
    let mut f_b = f(big_b);

    // Illinois variant of regula falsi. 100 iters is far past convergence
    // for any sane input — the σ solver typically converges in <10.
    for _ in 0..100 {
        let big_c = big_a + (big_a - big_b) * f_a / (f_b - f_a);
        let f_c = f(big_c);
        if f_c * f_b <= 0.0 {
            big_a = big_b;
            f_a = f_b;
        } else {
            f_a /= 2.0;
        }
        big_b = big_c;
        f_b = f_c;
        if (big_b - big_a).abs() < EPSILON {
            break;
        }
    }
    (big_a / 2.0).exp()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Glickman's worked example, §3.4: a player with rating 1500, RD 200,
    /// sigma 0.06 plays three opponents and wins/loses/loses. The expected
    /// post-period values are mu≈1464.06, phi≈151.52, sigma≈0.05999.
    #[test]
    fn glickman_worked_example() {
        let mut player = Rating { mu: 1500.0, phi: 200.0, sigma: 0.06 };
        let results = [
            PeriodResult { opponent_mu: 1400.0, opponent_phi:  30.0, score: 1.0 },
            PeriodResult { opponent_mu: 1550.0, opponent_phi: 100.0, score: 0.0 },
            PeriodResult { opponent_mu: 1700.0, opponent_phi: 300.0, score: 0.0 },
        ];
        update_player_with_results(&mut player, &results);
        assert!((player.mu - 1464.06).abs() < 0.5,
                "mu {} != 1464.06", player.mu);
        assert!((player.phi - 151.52).abs() < 0.5,
                "phi {} != 151.52", player.phi);
        assert!((player.sigma - 0.05999).abs() < 0.0005,
                "sigma {} != 0.05999", player.sigma);
    }

    #[test]
    fn inactive_player_only_inflates_rd() {
        let mut player = Rating { mu: 1700.0, phi: 100.0, sigma: 0.06 };
        decay_inactive_player(&mut player);
        assert_eq!(player.mu, 1700.0);
        assert_eq!(player.sigma, 0.06);
        assert!(player.phi > 100.0, "phi should grow for inactive player");
    }
}

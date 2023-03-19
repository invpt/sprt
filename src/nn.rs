use neun::{AdamOptimizer, Model, ModelDriver, Optimizer, OptimizerInstance, SgdOptimizer};
use rand::{seq::SliceRandom, Rng};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::{self, prelude::*, BufWriter};
use std::{fs::File, path::Path};

use crate::geometry::Rect;

#[derive(Serialize, Deserialize)]
pub struct TrainingParameters {
    pub hidden_layers: Vec<usize>,
    pub rects: Vec<Rect>,
    pub bounds: Rect,
    pub packing_size_min: usize,
    pub packing_size_max: usize,
    pub learning_rate_start: f32,
    pub learning_rate_end: f32,
    pub beta_1: f32,
    pub beta_2: f32,
    pub batch_size: usize,
    pub trials: usize,
    pub future_discount: f32,
    pub exploit_chance_start: f32,
    pub exploit_chance_end: f32,
    pub reward_threshold_start: f32,
    pub reward_threshold_end: f32,
}

pub fn store_weights(model: &Model, path: &Path) -> io::Result<()> {
    let mut weights_file = BufWriter::new(File::create(path)?);

    for var in model.variables() {
        weights_file.write_all(&var.to_be_bytes())?;
    }

    Ok(())
}

pub fn load_weights(model: &mut Model, path: &Path) -> io::Result<()> {
    if path.try_exists()? {
        // open the weights file
        let mut weights_file = File::open(path)?;

        // read the contnet
        let mut weights_bytes = vec![];
        weights_file.read_to_end(&mut weights_bytes).unwrap();

        // construct an iterator over the stored weights
        let vars = weights_bytes.chunks_exact(4).map(|chunk| {
            let mut buf = [0u8; 4];
            buf.copy_from_slice(chunk);
            f32::from_be_bytes(buf)
        });

        // update the weights with the loaded values
        model
            .variables_mut()
            .zip(vars)
            .for_each(|(mv, fv)| *mv = fv);
    }

    Ok(())
}

pub fn train_model(model: &mut Model, params: &TrainingParameters) {
    let mut driver = model.driver_mut();

    let mut rng = rand::thread_rng();
    let mut dx = vec![0.0; driver.model().variable_count()];
    let mut optimizer = AdamOptimizer {
        learning_rate: params.learning_rate_start,
        beta_1: params.beta_1,
        beta_2: params.beta_2,
    }
    .instance(driver.model().variable_count());

    let mut prediction_validity = VecDeque::<bool>::with_capacity(10_000);
    let mut trial = 0;
    let mut grads = 0;
    while trial < params.trials {
        let fraction_complete = trial as f32 / params.trials as f32;

        let exploit_chance = (1.0 - fraction_complete) * params.exploit_chance_start
            + fraction_complete * params.exploit_chance_end;

        let reward_threshold = (1.0 - fraction_complete) * params.reward_threshold_start
            + fraction_complete * params.exploit_chance_end;

        let packing = loop {
            let packing_size = rng.gen_range(params.packing_size_min..=params.packing_size_max);

            let chosen_rects = std::iter::repeat_with(|| params.rects.choose(&mut rng).unwrap())
                .take(packing_size);

            if let Some(packing) = find_packing(
                &mut rand::thread_rng(),
                &mut driver,
                &params.bounds,
                chosen_rects,
                exploit_chance,
            ) {
                break packing;
            }
        };

        let reward = reward(&params.bounds, &packing);

        if reward > reward_threshold {
            // status message
            if trial % (params.trials / 100) == 0 {
                eprintln!(
                    "{}% complete - {}% valid predictions",
                    trial / (params.trials / 100),
                    100.0
                        * prediction_validity
                            .iter()
                            .map(|&v| if v { 1.0 } else { 0.0 })
                            .sum::<f32>()
                        / prediction_validity.len() as f32,
                );
                eprintln!("variab: {:?}", driver.model().variables().sum::<f32>());
                eprintln!("varidx: {:?}", dx.iter().sum::<f32>())
            }

            trial += 1;

            let mut q = (reward - reward_threshold) / (1.0 - reward_threshold);

            for (i, rect) in packing.iter().enumerate().rev() {
                let packing_up_to = &packing[..i];

                let input = vectorize_input(&params.bounds, packing_up_to, rect.width(), rect.height());
                let target = vectorize_output(&params.bounds, rect.x1, rect.y1);

                let result = driver.run_and_record(&input);

                if trial % (params.trials / 100) == 0 {
                    eprintln!(" input: {:?}", input);
                    eprintln!("target: {:?}", target);
                    eprintln!("output: {:?}", result.output());
                }

                let (x1, y1) = devectorize_output(&params.bounds, result.output());
                let predicted = Rect {
                    x1,
                    y1,
                    x2: x1 + rect.width(),
                    y2: y1 + rect.height(),
                };
                let valid = packing_up_to.iter().all(|r| !r.overlaps(&predicted));
                if prediction_validity.len() == prediction_validity.capacity() {
                    prediction_validity.pop_front();
                }
                prediction_validity.push_back(valid);

                result.compute_gradients(&target, |idx, val| dx[idx] += q * val);
                grads += 1;

                if grads % params.batch_size == 0 {
                    /*eprintln!(" input: {:?}", input);
                    eprintln!("target: {:?}", target);
                    eprintln!("output: {:?}", result.output());
                    eprintln!("  vars: {:?}", driver.model().variables().sum::<f32>());*/
                    dx.iter_mut().for_each(|dx| *dx /= params.batch_size as f32);
                    optimizer.learning_rate = (1.0 - fraction_complete)
                        * params.learning_rate_start
                        + fraction_complete * params.learning_rate_end;
                    optimizer.apply(driver.model_mut().variables_mut().zip(dx.iter()));
                    dx.iter_mut().for_each(|dx| *dx = 0.0);
                }

                q *= params.future_discount;
            }
        }
    }
}

fn find_packing<'a>(
    rng: &mut impl rand::Rng,
    driver: &mut ModelDriver<&mut Model>,
    bounds: &Rect,
    rects: impl Iterator<Item = &'a Rect>,
    exploit_chance: f32,
) -> Option<Vec<Rect>> {
    let mut packing = Vec::<Rect>::with_capacity(rects.size_hint().0);

    for rect in rects {
        let input = vectorize_input(bounds, &packing, rect.width(), rect.height());

        let chosen_pos_index = if rng.gen_range(0.0..1.0) < exploit_chance {
            // attempt to find a position for the rectangle using the model
            let result = driver.run(&input);
            result
                .output()
                .iter()
                .enumerate()
                .reduce(|a, b| if a.1 > b.1 { a } else { b })
                .map(|(i, _)| i)
        } else {
            // choose one of the remaining places randomly
            let placeable = &input[..input.len() - 3];
            let count = placeable.iter().filter(|&&x| x == 1.0).count();
            placeable
                .iter()
                .enumerate()
                .filter(|(_, &x)| x == 1.0)
                .nth(rng.gen_range(0..count))
                .map(|(i, _)| i)
        };

        let Some(chosen_pos_index) = chosen_pos_index else { return None };

        let x1 = chosen_pos_index as i32 / bounds.height();
        let y1 = chosen_pos_index as i32 % bounds.height();

        let placed_rect = Rect {
            x1,
            y1,
            x2: x1 + rect.width(),
            y2: y1 + rect.height(),
        };

        if bounds.contains(&placed_rect) && packing.iter().all(|r| !r.overlaps(&placed_rect)) {
            packing.push(placed_rect);
        } else {
            return None;
        }
    }

    Some(packing)
}

pub fn evaluate_model<'a>(
    driver: &mut ModelDriver<&mut Model>,
    bounds: &Rect,
    samples: impl Iterator<Item = impl Iterator<Item = &'a Rect>>,
) {
    let mut samples_count = 0usize;
    let mut fails_count = 0usize;
    let mut total_reward = 0.0;

    for rects in samples {
        let mut packing = Vec::with_capacity(rects.size_hint().0);

        let mut success = true;
        for rect in rects {
            let input = vectorize_input(bounds, &packing, rect.width(), rect.height());
            let (x1, y1) = devectorize_output(bounds, driver.run(&input).output());
            let rect = Rect {
                x1,
                y1,
                x2: x1 + rect.width(),
                y2: y1 + rect.height(),
            };

            if bounds.contains(&rect) && packing.iter().all(|r| !r.overlaps(&rect)) {
                packing.push(rect);
            } else {
                success = false;
                break;
            }
        }

        if success {
            samples_count += 1;
            total_reward += reward(bounds, &packing);
        } else {
            fails_count += 1;
        }
    }

    println!(
        "Average reward on success: {}",
        total_reward / samples_count as f32
    );
    println!(
        "Success rate: {}%",
        100.0 * samples_count as f32 / (samples_count + fails_count) as f32
    );
}

fn vectorize_input(
    bounds: &Rect,
    packing: &[Rect],
    chosen_width: i32,
    chosen_height: i32,
) -> Vec<f32> {
    let buf_len = bounds.area() as usize + 2;
    let mut buf = vec![1.0; bounds.area() as usize + 2];

    for rect in packing {
        for x in (rect.x1 - chosen_width + 1).max(0)..rect.x2 {
            for y in (rect.y1 - chosen_height + 1).max(0)..rect.y2 {
                buf[(x * bounds.height() + y) as usize] = 0.0;
            }
        }
    }

    for x in (bounds.x2 - chosen_width + 1).max(0)..bounds.x2 {
        for y in bounds.y1..bounds.y2 {
            buf[(x * bounds.height() + y) as usize] = 0.0;
        }
    }

    for x in bounds.x1..bounds.x2 {
        for y in (bounds.y2 - chosen_height + 1).max(0)..bounds.y2 {
            buf[(x * bounds.height() + y) as usize] = 0.0;
        }
    }

    buf[buf_len - 2] = chosen_width as f32 / bounds.width() as f32;
    buf[buf_len - 1] = chosen_height as f32 / bounds.height() as f32;

    buf
}

fn vectorize_output(bounds: &Rect, chosen_x: i32, chosen_y: i32) -> Vec<f32> {
    let mut buf = vec![0.0; bounds.area() as usize];

    buf[(bounds.height() * chosen_x) as usize + chosen_y as usize] = 1.0;

    buf
}

fn devectorize_output(bounds: &Rect, output: &[f32]) -> (i32, i32) {
    let chosen_pos_index = output
        .iter()
        .enumerate()
        .reduce(|a, b| if a.1 > b.1 { a } else { b })
        .unwrap()
        .0;
    let chosen_x1 = chosen_pos_index as i32 / bounds.height();
    let chosen_y1 = chosen_pos_index as i32 % bounds.height();

    (chosen_x1, chosen_y1)
}

fn reward(bounds: &Rect, rects: &[Rect]) -> f32 {
    let worst = bounds.area();
    let best = {
        let max_width = rects.iter().map(Rect::width).max().unwrap();
        let max_height = rects.iter().map(Rect::height).max().unwrap();
        max_width * max_height
    };
    let actual = crate::heuristics::score::score(rects);

    ((worst - actual) as f32 / (worst - best) as f32).powi(2)
}

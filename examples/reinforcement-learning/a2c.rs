/* Advantage Actor Critic (A2C) model.
   A2C is a synchronous variant of Asynchronous the Advantage Actor Critic (A3C)
   model introduced by DeepMind in https://arxiv.org/abs/1602.01783

   See https://blog.openai.com/baselines-acktr-a2c/ for a reference
   python implementation.
*/
use super::gym_env::{GymEnv, Step};
use tch::kind::{FLOAT_CPU, INT64_CPU};
use tch::{nn, nn::OptimizerConfig, Device, Tensor};

static ENV_NAME: &'static str = "SpaceInvadersNoFrameskip-v4";
static NPROCS: i64 = 16;
static NSTEPS: i64 = 5;
static NSTACK: i64 = 4;
static UPDATES: i64 = 1000000;

fn model(p: &nn::Path, nact: i64) -> Box<Fn(&Tensor) -> (Tensor, Tensor)> {
    let stride = |s| nn::ConvConfig {
        stride: s,
        ..Default::default()
    };
    let seq = nn::Sequential::new()
        .add(nn::Conv2D::new(p / "c1", NSTACK, 32, 8, stride(4)))
        .add(nn::Conv2D::new(p / "c2", 32, 64, 4, stride(2)))
        .add(nn::Conv2D::new(p / "c3", 64, 64, 3, stride(1)))
        .add(nn::Linear::new(p / "l1", 3136, 512, Default::default()));
    let critic = nn::Linear::new(p / "cl", 512, 1, Default::default());
    let actor = nn::Linear::new(p / "al", 512, nact, Default::default());
    Box::new(move |xs: &Tensor| {
        let xs = xs.apply(&seq);
        (xs.apply(&critic), xs.apply(&actor))
    })
}

#[derive(Debug)]
struct FrameStack {
    data: Tensor,
    nprocs: i64,
    nstack: i64,
}

impl FrameStack {
    fn new(nprocs: i64, nstack: i64) -> FrameStack {
        FrameStack {
            data: Tensor::zeros(&[nprocs, nstack, 84, 84], FLOAT_CPU),
            nprocs,
            nstack,
        }
    }

    fn update<'a>(&'a mut self, img: &Tensor) -> &'a Tensor {
        let slice = |i| self.data.narrow(1, i, 1);
        for i in 1..self.nstack {
            slice(i - 1).copy_(&slice(i))
        }
        slice(self.nstack - 1).copy_(img);
        &self.data
    }
}

pub fn run() -> cpython::PyResult<()> {
    let env = GymEnv::new(ENV_NAME, Some(NPROCS))?;
    println!("action space: {}", env.action_space());
    println!("observation space: {:?}", env.observation_space());

    let device = tch::Device::cuda_if_available();
    let vs = nn::VarStore::new(device);
    let model = model(&vs.root(), env.action_space());
    let opt = nn::Adam::default().build(&vs, 1e-2).unwrap();

    let mut frame_stack = FrameStack::new(NPROCS, NSTACK);
    let _ = frame_stack.update(&env.reset()?);
    let s_states = Tensor::zeros(&[NSTEPS + 1, NPROCS, NSTACK, 84, 84], FLOAT_CPU);
    let s_values = Tensor::zeros(&[NSTEPS, NPROCS], FLOAT_CPU);
    let s_rewards = Tensor::zeros(&[NSTEPS, NPROCS], FLOAT_CPU);
    let s_actions = Tensor::zeros(&[NSTEPS, NPROCS], INT64_CPU);
    let s_masks = Tensor::zeros(&[NSTEPS, NPROCS], FLOAT_CPU);
    for _update_index in 0..UPDATES {
        for s in 0..NSTEPS {
            let (critic, actor) = tch::no_grad(|| model(&s_states.get(s)));
            let probs = actor.softmax(-1);
            let actions = probs.multinomial(1, true).squeeze1(-1);
            let step = env.step(Vec::<i64>::from(&actions)[0])?; // TODO
            let obs = Tensor::from(42.0); // TODO: obs/frame-stack
            let is_done = Tensor::from(42.0); // TODO
            let masks = Tensor::from(1.) - is_done;
            s_actions.get(s).copy_(&actions);
            s_values.get(s).copy_(&critic.squeeze1(-1));
            s_states.get(s + 1).copy_(&obs);
            s_rewards.get(s).copy_(&Tensor::float_vec(&[0.0])); // TODO
            s_masks.get(s).copy_(&masks);
        }
        let s_returns = {
            let r = Tensor::zeros(&[NSTEPS + 1, NPROCS], FLOAT_CPU);
            let critic = tch::no_grad(|| model(&s_states.get(-1)).0);
            r.get(-1).copy_(&critic.view(&[NPROCS]));
            for s in (0..NSTEPS - 1).rev() {
                let r_s = s_rewards.get(s) + r.get(s + 1) * s_masks.get(s) * 0.99;
                r.get(s).copy_(&r_s);
            }
            r
        };
        let (critic, actor) =
            model(
                &s_states
                    .narrow(0, 0, NSTEPS)
                    .view(&[NSTEPS * NPROCS, NSTACK, 84, 84]),
            );
        let critic = critic.view(&[NSTEPS, NPROCS]);
        let actor = actor.view(&[NSTEPS, NPROCS, -1]);
        let log_probs = actor.log_softmax(-1);
        let probs = actor.softmax(-1);
        let action_log_probs = {
            let index = s_actions.unsqueeze(-1).to_device(device);
            log_probs.gather(2, &index).squeeze1(-1)
        };
        let dist_entropy = (-log_probs * probs).sum2(&[-1], false).mean();
        let advantages = s_returns.narrow(0, 0, NSTEPS).to_device(device) - critic;
        let value_loss = (&advantages * &advantages).mean();
        let action_loss = (-advantages.detach() * action_log_probs).mean();
        let loss = value_loss * 0.5 + action_loss - dist_entropy * 0.01;
        opt.backward_step(&loss); // TODO: clip gradient to 0.5
    }
    Ok(())
}

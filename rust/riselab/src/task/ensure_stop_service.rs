use anyhow::Result;

use super::{ExecuteContext, Task};

pub struct EnsureStopService {
    ports: Vec<u16>,
}

impl EnsureStopService {
    pub fn new(ports: Vec<u16>) -> Result<Self> {
        Ok(Self { ports })
    }
}

impl Task for EnsureStopService {
    fn execute(&mut self, ctx: &mut ExecuteContext<impl std::io::Write>) -> anyhow::Result<()> {
        ctx.service(self);

        for port in &self.ports {
            let address = format!("127.0.0.1:{}", port);

            ctx.pb
                .set_message(format!("waiting for port close - {}", address));
            ctx.wait_tcp_close(&address)?;
        }

        ctx.pb
            .set_message("all previous services have been stopped");

        ctx.complete_spin();

        Ok(())
    }

    fn id(&self) -> String {
        "prepare".into()
    }
}

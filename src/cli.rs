use clap::Parser;

#[derive(Parser)]
#[command(name = "player", about = "Zenoh video stream player")]
pub struct Args {
    /// Zenoh endpoint address (e.g. tcp/192.168.31.113:7447)
    #[arg(short = 'e', long, default_value = "tcp/192.168.31.113:7447")]
    pub endpoint: String,

    /// Zenoh topic to subscribe to
    #[arg(short, long, default_value = "video/RadCam19216831100/stream")]
    pub topic: String,
}

use cpal::traits::{DeviceTrait, HostTrait, EventLoopTrait};
use cpal::{StreamData, UnknownTypeInputBuffer};
use tokio::sync::{watch, mpsc, Mutex};
use std::thread;
use audiopus::coder::Encoder;
use audiopus::SampleRate;
use audiopus::Channels;
use audiopus::Application;
use audiopus::packet::MutPacket;
use audiopus::TryFrom;
//use audiopus::TryInto;
use audiopus::MutSignals;
use futures::stream::Chunks;
use std::convert::TryInto;
use std::clone::Clone;

use anyhow::Error;
use anyhow::anyhow;


type Tx = tokio::sync::watch::Sender<u8>;

async fn getSupportedFormat(s: cpal::SupportedFormat) -> Result<cpal::Format, anyhow::Error> {
    let opusrates = [48000, 24000, 16000, 12000, 8000];
    let min = &s.min_sample_rate.0;
    let max = &s.max_sample_rate.0;

    for samp in opusrates.into_iter() {
        if samp >= min && samp <= max {
            return Ok(cpal::Format{channels: s.channels,
                                  sample_rate: cpal::SampleRate(*samp),
                                  data_type: s.data_type,})

        }
    }
    return Err(anyhow!("no avaliable formats!"));
}

pub async fn start(tx: Tx ) -> Result<(), anyhow::Error>{
    //split tcp server, pass rx to mutex arc
    tokio::spawn(async move {
    let host = cpal::default_host();
    let event_loop = host.event_loop();

    //initialize opus codec

    let device = host.default_output_device().expect("no output device available");
    let mut supported_formats_range = device.supported_output_formats()
    .expect("error while querying formats");

    let supformat = supported_formats_range.next()
    .expect("no supported format?!");



    let format = getSupportedFormat(supformat.clone()).await.unwrap();

    let stream_id = event_loop.build_input_stream(&device, &format).unwrap();
    event_loop.play_stream(stream_id).expect("failed to play_stream");

    println!("{:?}", format.sample_rate.0);
    

    let enc = Encoder::new(SampleRate::try_from(format.sample_rate.0.try_into().unwrap()).unwrap(),
                           Channels::Mono,
                           Application::Voip).unwrap();

    thread::spawn(move || { //use chunks.await
        let mut hold: [f32; 1920] = [0.0; 1920];
        let mut countt: usize = 0;
            event_loop.run(move |_stream_id, _stream_result| {
        let stream_data = match _stream_result {
            Ok(data) => data,
            Err(err) => {
                eprintln!("error");
                return;
            }
        };

        match stream_data {
            StreamData::Input {buffer: UnknownTypeInputBuffer::U16(buffer)} => {
                for elem in buffer.iter() {
                    println!("{:?}", elem);
                    println!("u16")
                }
            },
            StreamData::Input {buffer: UnknownTypeInputBuffer::I16(buffer)} => {
                for elem in buffer.iter() {
                    println!("{:?}", elem);
                    println!("i16")
                }
            },
            StreamData::Input {buffer: UnknownTypeInputBuffer::F32(buffer)} => {
                    let opus_out: &mut [u8; 100] = &mut [0; 100];
                    let tempf32: &mut [f32] = &mut [0.0; 1920];
                for elem in buffer.chunks(500) {
                    //map the f32 to i16 properly
                    //let num: i16 = (elem.clone() * (i16::max_value() as f32)) as i16;
                    //at 48kh 4800 = 1s we use 1920 samples per frame
                    //packet
                    //outtt[countt..elem.len()+countt].copy_from_slice(&elem);
                    //countt += elem.len();
                    //println!("{:?}", countt);

                    if elem.len() + countt >= 1920 {
                        tempf32[..countt].copy_from_slice(&hold[..countt]);
                        tempf32[countt..].copy_from_slice(&elem[0..1920-countt]);
                        let cur = countt + elem.len();

                        countt = cur % 1920;
                        hold[..countt].clone_from_slice(&elem[elem.len()-countt..]);
                    } else {
                        hold[countt..elem.len()+countt].clone_from_slice(&elem);
                        countt += elem.len();
                        continue;
                    }
                    let a = enc.encode_float(tempf32, opus_out).unwrap();
                    let b = &opus_out[1..a];
                    for byte in b.iter() {
                        let _ = tx.broadcast(*byte);
                    }
                }
            },
            _ => (),
        }
});
    });
    });
    Ok(())

}

async fn process() {

}
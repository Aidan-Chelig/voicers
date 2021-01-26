use cpal::traits::{DeviceTrait, HostTrait, EventLoopTrait};
use cpal::{StreamData, UnknownTypeInputBuffer};
use tokio::sync::{watch, Mutex};
use std::sync::mpsc;
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
use futures::executor::block_on;// USBLOCK ON
use std::convert::TryInto;
use std::clone::Clone;

use anyhow::Error;
use anyhow::anyhow;


/*
on peer recieve put bytes into vec of vecs [[]] one vec for every peer. then on output eventloop read from that vec of vecs and mix audio
*/

type InputSender = tokio::sync::watch::Sender<u8>;
type InputReceiver = tokio::sync::watch::Receiver<u8>;
type OutputSender = std::sync::mpsc::Sender<u8>;
type OutputReceiver = std::sync::mpsc::Receiver<u8>;

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

pub async fn start() -> Result<InputReceiver, anyhow::Error>{
    //split tcp server, pass rx to mutex arc
    let (itx, irx) = watch::channel::<u8>(0);
    let (otx, orx) = mpsc::channel::<u8>();

    let host = cpal::default_host();

    let input_event_loop = host.event_loop();
    let output_event_loop = host.event_loop();

    //initialize opus codec

    let input_device = host.default_input_device().expect("no input device available");
    let output_device = host.default_output_device().expect("no output device available");

    let mut input_supported_formats_range = input_device.supported_input_formats()
    .expect("error while querying input formats");

    let mut output_supported_formats_range = output_device.supported_output_formats()
    .expect("error while querying output formats");


    let input_supformat = input_supported_formats_range.next()
    .expect("no supported input format?!");

    let output_supformat = output_supported_formats_range.next()
    .expect("no supported output format?!");


    let input_format = getSupportedFormat(input_supformat.clone()).await.unwrap();
    let output_format = getSupportedFormat(output_supformat.clone()).await.unwrap();

    let input_stream_id = input_event_loop.build_input_stream(&input_device, &input_format).unwrap();
    input_event_loop.play_stream(input_stream_id).expect("failed to play_stream");

    let output_stream_id = output_event_loop.build_output_stream(&output_device, &output_format).unwrap();
    input_event_loop.play_stream(output_stream_id).expect("failed to play_stream");

    println!("{:?}", input_format.sample_rate.0);
    println!("{:?}", output_format.sample_rate.0);
    

    let enc = Encoder::new(SampleRate::try_from(input_format.sample_rate.0.try_into().unwrap()).unwrap(),
                           Channels::Mono,
                           Application::Voip).unwrap();

    thread::spawn(move || { //use chunks.await
        let mut hold: [f32; 1920] = [0.0; 1920];
        let mut countt: usize = 0;
        // block_on()
            //event_loop.run(move |_stream_id, _stream_result| {
            input_event_loop.run(move |_stream_id, _stream_result| { // need to understand how to get this to work
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
                        let _ = itx.broadcast(*byte);
                    }
                }
            },
            _ => (),
        }

            });



    });
    Ok(irx)

}

fn processInput() {
    
}

async fn startOutput() -> Result<OutputSender, anyhow::Error> {
    let (tx, rx) = mpsc::channel::<u8>();
        tokio::spawn(async move {
        });
    //start audio out processing


    Ok(tx)
}

async fn process() {

}
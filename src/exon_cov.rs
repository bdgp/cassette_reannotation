use std::str;
use std::vec::Vec;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::io::stdout;
use std::ops::Range;
use anyhow::{Result, anyhow};

use cassette_reannotation::*;
use cassette_reannotation::indexed_annotation::*;

use rust_htslib::bam::Read;
use rust_htslib::bam::IndexedReader;

use structopt::StructOpt;

use futures_cpupool::CpuPool;
use futures::future::Future;

use ordered_float::OrderedFloat;

#[derive(StructOpt, Debug)]
#[structopt(name = "intronrpkm", about = "Analyze RPKM values in intronic space")]
struct Options {
    // input files
    #[structopt(long="gff", help = "A genome annotation file in gff3 format", name="ANNOT_GFF_FILE")]
    annotfile_gff: Option<String>,
    #[structopt(long="gtf", help = "A genome annotation file in gtf format", name="ANNOT_GTF_FILE")]
    annotfile_gtf: Option<String>,
    #[structopt(long="bam1", short="1", help = "The set of stranded .bam files to analyze where read1 indicates strand", name="BAMFILE1")]
    bam1: Vec<String>,
    #[structopt(long="bam2", short="2", help = "The set of stranded .bam files to analyze where read2 indicates strand", name="BAMFILE2")]
    bam2: Vec<String>,
    #[structopt(long="bam", short="u", help = "The set of unstranded .bam files to analyze", name="BAMFILE")]
    bam: Vec<String>,
    #[structopt(long="chrmap", help = "Optional tab-delimited chr name mapping file", name="CHRMAP_FILE")]
    chrmap_file: Option<String>,
    #[structopt(long="vizchrmap", help = "Optional tab-delimited chr name mapping file for bigwig/bigbed exports", name="VIZCHRMAP_FILE")]
    vizchrmap_file: Option<String>,
    #[structopt(long="sizes", help = "Optional chr sizes file", name="SIZES_FILE")]
    sizes_file: Option<String>,
    // output file
    #[structopt(long="out", short="o", help = "Output file", name="OUT_FILE", default_value="-")]
    outfile: String,
    #[structopt(long="merged", short="m", help = "Merged Output file", name="MERGED_OUT_FILE")]
    merged_outfile: Option<String>,
    // feature types filter
    #[structopt(long="exon_type", help = "The exon type(s) to search for", name="EXON_TYPE")]
    exon_type: Vec<String>,
    #[structopt(long="transcript_type", help = "The transcript type(s) to search for", name="TRANSCRIPT_TYPE")]
    transcript_type: Vec<String>,
    #[structopt(long="gene_type", help = "The gene type(s) to search for", name="GENE_TYPE")]
    gene_type: Vec<String>,
    // flags
    #[structopt(long="cpu_threads", short="t", help = "How many threads to use for processing", default_value="0")]
    cpu_threads: usize,
}

#[derive(Ord, Eq, PartialOrd, PartialEq)]
struct Row {
    seqname: String,
    strand: String,
    start: u64,
    end: u64,
    cov: OrderedFloat<f64>,
    rpkm: OrderedFloat<f64>,
    transcript_id: String,
    gene_id: String
}

fn write_exon_cov(
    options: &Options,
    annot: &Arc<IndexedAnnotation>,
    total_reads: u64,
    bamfiles: &Vec<String>,
    bamstrand: &Vec<Option<bool>>)
    -> Result<()>
{
    let mut tidmaps = HashMap::<String,HashMap<String,u32>>::new();
    for bamfile in bamfiles {
        let bam = IndexedReader::from_path(bamfile)?;
        // build the tid map for this bam file
        let mut tidmap = HashMap::<String,u32>::new();
        {   let header = bam.header();
            for target_name in header.target_names() {
                let tid = header.tid(target_name).ok_or(anyhow!("NoneError"))?;
                let target_name = String::from(std::str::from_utf8(target_name)?);
                let chr = annot.chrmap.get(&target_name).unwrap_or(&target_name);
                tidmap.insert(chr.clone(), tid);
            }
        }
        tidmaps.insert(bamfile.clone(), tidmap);
    }
    let tidmaps = Arc::new(tidmaps);

    let mut unmerged_exons = HashMap::<(String,String),Vec<(Range<u64>,Option<usize>)>>::new();
    for (gene_row, gene) in annot.rows.iter().enumerate() {
        if options.gene_type.is_empty() || options.gene_type.contains(&gene.feature_type) {
            if let Some(feature_rows) = annot.row2children.get(&gene_row) {
                // get the transcript rows for this gene
                'transcript_row:
                for transcript_row in feature_rows {
                    let transcript = &annot.rows[*transcript_row];
                    // make sure this is a transcript type
                    // make sure transcript coordinates are consistent with the gene coordinates
                    if (options.transcript_type.is_empty() ||
                            options.transcript_type.contains(&transcript.feature_type)) &&
                        transcript.seqname == gene.seqname &&
                        transcript.strand == gene.strand
                    {
                        if let Some(exon_rows) = annot.row2children.get(transcript_row) {
                            // first get exon start/stop -> transcript associations
                            // and splice start/stop -> transcript associations
                            for exon_row in exon_rows {
                                let exon = &annot.rows[*exon_row];
                                if options.exon_type.is_empty() || options.exon_type.contains(&exon.feature_type) {
                                    let exon = &annot.rows[*exon_row];
                                    unmerged_exons.entry((exon.seqname.clone(),exon.strand.clone())).
                                        or_insert_with(Vec::new).push((exon.start-1..exon.end, Some(*exon_row)));
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let mut merged_exons = HashMap::<(String,String),Vec<(Range<u64>,Option<usize>)>>::new();
    let keys = unmerged_exons.keys().map(|k| k.clone()).collect::<Vec<_>>();
    for key in &keys {
        if let Some(ref mut unmerged) = unmerged_exons.get_mut(&key) {
            unmerged.sort_by_key(|u| u.0.start);
            let merged = merged_exons.entry((*key).clone()).or_insert_with(Vec::new);
            for exon in unmerged.iter() {
                let mut extend = false;
                if let Some(ref mut last) = merged.last_mut() {
                    if exon.0.start < last.0.end && last.0.start < exon.0.end {
                        last.0.end = exon.0.end;
                        extend = true;
                    }
                }
                if !extend {
                    merged.push((exon.0.clone(), None));
                }
            }
        }
    }

    write_exon_cov_to_file(options,
        total_reads,
        &unmerged_exons,
        &options.outfile,
        bamfiles,
        bamstrand,
        &tidmaps,
        &annot,
    )?;
    if let Some(ref merged_outfile) = options.merged_outfile {
        write_exon_cov_to_file(options,
            total_reads,
            &merged_exons,
            &merged_outfile,
            bamfiles,
            bamstrand,
            &tidmaps,
            &annot,
        )?;
    }
    Ok(())
}

fn write_exon_cov_to_file(
    options: &Options,
    total_reads: u64,
    exons: &HashMap<(String,String),Vec<(Range<u64>,Option<usize>)>>,
    outfile: &str,
    bamfiles: &Vec<String>,
    bamstrand: &Vec<Option<bool>>,
    tidmaps: &Arc<HashMap<String,HashMap<String,u32>>>,
    annot: &Arc<IndexedAnnotation>) 
    -> Result<()> 
{
    let mut output: BufWriter<Box<dyn Write>> = BufWriter::new(
        if outfile == "-" { Box::new(stdout()) }
            else { Box::new(File::create(&outfile)?) });

    let num_cpus = num_cpus::get();
    let mut pair_futures = Vec::new();
    let pool = Arc::new(CpuPool::new(if options.cpu_threads==0 {num_cpus} else {options.cpu_threads}));
    for key in exons.keys() {
        let exons = exons[&key.clone()].clone();
        for exon in exons {
            let seqname = key.0.clone();
            let strand = key.1.clone();
            let chr = seqname.clone();
            let strand_is_plus = strand == "+";
            let bamfiles = bamfiles.clone();
            let bamstrand = bamstrand.clone();
            let tidmaps = tidmaps.clone();
            let annot = annot.clone();

            let pair_future = pool.spawn_fn(move ||->Result<Row> {
                let mut exon_cov = 0f64;
                let mut exon_reads = HashSet::<String>::new();
                for (i,bamfile) in bamfiles.iter().enumerate() {
                    let read1strand = bamstrand[i];
                    let tidmap = &tidmaps[bamfile];
                    if let Some(tid) = tidmap.get(&chr) {
                        let mut bam = IndexedReader::from_path(bamfile)?;
                        bam.fetch(*tid, exon.0.start as u32, exon.0.end as u32)?;
                        for read in bam.records() {
                            let read = read?;
                            // make sure the read's strand matches
                            if let Some(is_read1strand) = read1strand {
                                if !(((is_read1strand == read.is_first_in_template()) == !read.is_reverse()) == strand_is_plus) {
                                    continue;
                                }
                            }
                            let read_name = String::from(str::from_utf8(read.qname())?);
                            exon_reads.insert(read_name);
                            //eprintln!("Looking at read: {}", read_name);
                            let exons = cigar2exons(&read.cigar(), read.pos() as u64)?;
                            for e in exons {
                                if e.start < exon.0.end && exon.0.start < e.end {
                                    exon_cov +=
                                        (std::cmp::min(e.end, exon.0.end) -
                                        std::cmp::max(e.start, exon.0.start)) as f64;
                                }
                            }
                        }
                    }
                }
                let exon_length = exon.0.end-exon.0.start;
                let rpkm = (1e10f64 * exon_reads.len() as f64) / (total_reads as f64 * exon_length as f64);
                let gene_id = match exon.1 {
                    Some(exon_row) => match get_gene_name(exon_row, &annot) {
                        Some(name) => name,
                        None => "".to_string(),
                    }
                    None => "".to_string(),
                };
                let transcript_id = match exon.1 {
                    Some(exon_row) => match get_name(exon_row, &annot) {
                        Some(name) => name,
                        None => "".to_string(),
                    }
                    None => "".to_string(),
                };
                Ok(Row{
                    seqname: seqname,
                    strand: strand,
                    start: exon.0.start,
                    end: exon.0.end,
                    cov: OrderedFloat(exon_cov / exon_length as f64),
                    rpkm: OrderedFloat(rpkm),
                    transcript_id: transcript_id,
                    gene_id: gene_id,
                })
            });
            pair_futures.push(pair_future);
        }
    }

    let mut rows = Vec::new();
    for future in pair_futures {
        match future.wait() {
            Ok(row) => {
                rows.push(row)
            }
            Err(ref e) => {
                eprintln!("Got Err in write_exon_cov: {:?}", e);
            }
        }
    }
    rows.sort();

    output.write_fmt(format_args!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                                  "seqname",
                                  "strand",
                                  "start",
                                  "end",
                                  "cov",
                                  "rpkm",
                                  "transcript_id",
                                  "gene_id",
    ))?;
    for row in rows {
        output.write_fmt(format_args!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            row.seqname,
            row.strand,
            row.start,
            row.end,
            row.cov,
            row.rpkm,
            row.transcript_id,
            row.gene_id,
        ))?;
    }
    Ok(())
}

fn run() -> Result<()> {
    let mut options = Options::from_args();
    // set defaults for feature types
    options.gene_type =
        (if options.gene_type.is_empty() { vec!["gene".to_string()] } 
         else { options.gene_type.clone() }).into_iter().collect();
    //options.transcript_type = 
    //    (if options.transcript_type.is_empty() { vec!["transcript".to_string()] } 
    //     else { options.transcript_type.clone() }).into_iter().collect();
    options.exon_type =
        (if options.exon_type.is_empty() { vec!["exon".to_string()] }
         else { options.exon_type.clone() }).into_iter().collect();
    // set debug options if --debug flag is set

    // organize bamfiles by read strand type
    let mut bamfiles = Vec::<String>::new();
    bamfiles.append(&mut options.bam1.clone());
    bamfiles.append(&mut options.bam2.clone());
    bamfiles.append(&mut options.bam.clone());
    let mut bamstrand = Vec::<Option<bool>>::new();
    bamstrand.append(&mut options.bam1.iter().map(|_| Some(true)).collect());
    bamstrand.append(&mut options.bam2.iter().map(|_| Some(false)).collect());
    bamstrand.append(&mut options.bam.iter().map(|_| None).collect());
    // get the chromosome names and sizes from the first bam file
    if bamfiles.is_empty() {
        Options::clap().print_help()?;
        eprintln!("\n\nNo bam files were passed in!");
        std::process::exit(1)
    }
    let transcript_type = String::from("transcript");
    let mut annot = if let Some(annotfile_gff) = options.annotfile_gff.clone() {
        eprintln!("Reading annotation file {:?}", &annotfile_gff);
        IndexedAnnotation::from_gff(
            &annotfile_gff, 
            &options.chrmap_file,
            &options.vizchrmap_file)?
    } else if let Some(annotfile_gtf) = options.annotfile_gtf.clone() {
        eprintln!("Reading annotation file {:?}", &annotfile_gtf);
        IndexedAnnotation::from_gtf(&annotfile_gtf, 
            options.gene_type.get(0).ok_or(anyhow!("NoneError"))?,
            options.transcript_type.get(0).unwrap_or(&transcript_type),
            &options.chrmap_file,
            &options.vizchrmap_file)?
    } else {
        Options::clap().print_help()?;
        eprintln!("\n\nNo annotation file was given!");
        std::process::exit(1);
    };
    eprintln!("Getting refseq lengths from bam file {:?}", &bamfiles[0]);
    let refs = match options.sizes_file.clone() {
        Some(sizes_file) => read_sizes_file(&sizes_file, &annot.chrmap)?,
        None => get_bam_refs(&bamfiles[0], &annot.chrmap)?,
    };
    annot.refs = refs;
    
    // get the total bam reads
    eprintln!("Running samtools idxstats to get total bam read counts");
    let total_reads = get_bam_total_reads(&bamfiles)?;
    eprintln!("Found {} total reads", total_reads);
    write_exon_cov(&options, &Arc::new(annot), total_reads, &bamfiles, &bamstrand)?;
    Ok(())
}

fn main() -> Result<()> {
    // enable stack traces
    std::env::set_var("RUST_BACKTRACE", "full");
    run()
}

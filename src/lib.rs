use std::vec::Vec;
use std::ops::Range;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufRead};
use linked_hash_map::LinkedHashMap;
use rust_htslib::bam::record::Cigar;
use rust_htslib::bam::record::CigarStringView;
use rust_htslib::bam::Read;
use rust_htslib::bam::IndexedReader;
use anyhow::{Result, anyhow};
use duct::cmd;

pub mod indexed_annotation;

pub mod power_set {
    pub struct PowerSet<'a, T: 'a> {
        source: &'a [T],
        position: usize
    }
    
    impl<'a, T> PowerSet<'a, T> where T: Clone {
        pub fn new(source: &'a [T]) -> PowerSet<'a, T> {
            PowerSet { source: source, position: 0 }
        }
    }

    impl<'a, T> Iterator for PowerSet<'a, T> where T: Clone {
        type Item = Vec<T>;

        fn next(&mut self) -> Option<Self::Item> {
            if 2usize.pow(self.source.len() as u32) <= self.position {
                None
            } else {
                let res = self.source.iter().enumerate().filter(|&(i, _)| (self.position >> i) % 2 == 1)
                                                        .map(|(_, element)| element.clone()).collect();
                self.position = self.position + 1;
                Some(res)
            }
        }
    }
}

pub fn cigar2exons(cigar: &CigarStringView, pos: u64) -> Result<Vec<Range<u64>>> {
    let mut exons = Vec::<Range<u64>>::new();
    let mut pos = pos;
    for op in cigar {
        match op {
            &Cigar::Match(length) => {
                pos += length as u64;
                if length > 0 {
                    exons.push(Range{start: pos - length as u64, end: pos});
                }
            }
            &Cigar::RefSkip(length) |
            &Cigar::Del(length) |
            &Cigar::Equal(length) |
            &Cigar::Diff(length) => {
                pos += length as u64;
            }
            &Cigar::Ins(_) |
            &Cigar::SoftClip(_) |
            &Cigar::HardClip(_) |
            &Cigar::Pad(_) => (),
        };
    }
    Ok(exons)
}

pub fn read_sizes_file(sizes_file: &str, chrmap: &HashMap<String,String>) -> Result<LinkedHashMap<String,u64>> {
    let mut refs = HashMap::<String,u64>::new();
    let f = File::open(&sizes_file)?;
    let mut file = BufReader::new(&f);
    let mut buf = String::new();
    while file.read_line(&mut buf)? > 0 {
        {   let line = buf.trim_end_matches('\n').trim_end_matches('\r');
            let cols: Vec<&str> = line.split('\t').collect();
            if let Some(chr) = cols.get(0) {
                let chr = String::from(*chr);
                let chr = chrmap.get(&chr).unwrap_or(&chr);
                if let Some(size) = cols.get(1) {
                    if let Ok(size) = size.parse::<u64>() {
                        refs.insert(chr.clone(), size);
                    }
                    else {
                        anyhow!("Could not parse size \"{}\" for chr \"{}\" from line \"{}\" of file \"{}\"", size, chr, line, sizes_file);
                    }
                }
            }
        }
        buf.clear();
    }
    let mut chrs = refs.keys().collect::<Vec<_>>();
    chrs.sort_by_key(|a| a.as_bytes());
    let mut sorted_refs = LinkedHashMap::<String,u64>::new();
    for chr in chrs {
        let size = refs[chr];
        sorted_refs.insert(chr.clone(), size);
    }
    Ok(sorted_refs)
}

pub fn get_bam_refs(bamfile: &str, chrmap: &HashMap<String,String>) -> Result<LinkedHashMap<String,u64>> {
    let mut refs = LinkedHashMap::<String,u64>::new();
    let bam = IndexedReader::from_path(bamfile)?;
    let header = bam.header();
    let target_names = header.target_names();
    for target_name in target_names {
        let tid = header.tid(target_name).ok_or(anyhow!("NoneError"))?;
        let target_len = header.target_len(tid).ok_or(anyhow!("NoneError"))? as u64;
        let target_name = String::from(std::str::from_utf8(target_name)?);
        let chr = chrmap.get(&target_name).unwrap_or(&target_name);
        refs.insert(chr.clone(), target_len);
    }
    Ok(refs)
}

pub fn get_bam_total_reads(bamfiles: &[String]) -> Result<u64> {
    let mut total_reads = 0u64;
    for bamfile in bamfiles {
        let stdout = cmd!("samtools","idxstats",bamfile).read()?;
        for line in stdout.lines() {
            let cols: Vec<&str> = line.split('\t').collect();
            if let Some(reads_str) = cols.get(2) {
                if let Ok(reads) = reads_str.parse::<u64>() {
                    total_reads += reads;
                }
            }
        }
    }
    Ok(total_reads)
}

pub fn get_gene_name(row: usize, annot: &indexed_annotation::IndexedAnnotation) -> Option<String> {
    let name =
            annot.rows[row].attributes.get("Name").or_else(||
            annot.rows[row].attributes.get("ID").or_else(||
            annot.rows[row].attributes.get("gene_name").or_else(||
            annot.rows[row].attributes.get("gene").or_else(||
            annot.rows[row].attributes.get("gene_id")))));
    name.map(|n| n.to_string())
}

pub fn get_name(row: usize, annot: &indexed_annotation::IndexedAnnotation) -> Option<String> {
    let name =
        annot.rows[row].attributes.get("transcript_name").or_else(||
        annot.rows[row].attributes.get("transcript").or_else(||
        annot.rows[row].attributes.get("Name").or_else(||
        annot.rows[row].attributes.get("ID").or_else(||
        annot.rows[row].attributes.get("transcript_id").or_else(||
        annot.rows[row].attributes.get("gene_name").or_else(||
        annot.rows[row].attributes.get("gene").or_else(||
        annot.rows[row].attributes.get("gene_id"))))))));
    name.map(|n| n.to_string())
}


use std::{io::Read, sync::Arc};

use parquet_format::{CompressionCodec, PageHeader, PageType};
use thrift::protocol::TCompactInputProtocol;

use crate::schema::types::ParquetType;
use crate::{errors::Result, metadata::ColumnDescriptor};

use super::page::{Page, PageV1, PageV2};
use super::page_dict::{read_page_dict, PageDict};

/// A page iterator iterates over row group's pages. In parquet, pages are guaranteed to be
/// contiguously arranged in memory and therefore must be read in sequence.
#[derive(Debug)]
pub struct PageIterator<'a, R: Read> {
    // The source
    reader: &'a mut R,

    compression: CompressionCodec,

    // The number of values we have seen so far.
    seen_num_values: i64,

    // The number of total values in this column chunk.
    total_num_values: i64,

    // Arc: it will be shared between multiple pages and pages should be Send + Sync.
    current_dictionary: Option<Arc<dyn PageDict>>,

    //
    descriptor: ColumnDescriptor,
}

impl<'a, R: Read> PageIterator<'a, R> {
    pub fn try_new(
        reader: &'a mut R,
        total_num_values: i64,
        compression: CompressionCodec,
        descriptor: ColumnDescriptor,
    ) -> Result<Self> {
        Ok(Self {
            reader,
            total_num_values,
            compression,
            seen_num_values: 0,
            current_dictionary: None,
            descriptor,
        })
    }

    /// Reads Page header from Thrift.
    fn read_page_header(&mut self) -> Result<PageHeader> {
        let mut prot = TCompactInputProtocol::new(&mut self.reader);
        let page_header = PageHeader::read_from_in_protocol(&mut prot)?;
        Ok(page_header)
    }

    pub fn descriptor(&self) -> &ColumnDescriptor {
        &self.descriptor
    }
}

impl<'a, R: Read> Iterator for PageIterator<'a, R> {
    type Item = Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        next_page(self).transpose()
    }
}

/// This function is lightweight and executes a minimal amount of work so that it is IO bounded.
// Any un-necessary CPU-intensive tasks SHOULD be executed on individual pages.
fn next_page<R: Read>(reader: &mut PageIterator<R>) -> Result<Option<Page>> {
    while reader.seen_num_values < reader.total_num_values {
        let page_header = reader.read_page_header()?;

        let mut buffer = vec![0; page_header.compressed_page_size as usize];
        reader.reader.read_exact(&mut buffer)?;

        let result = match page_header.type_ {
            PageType::DictionaryPage => {
                let dict_header = page_header.dictionary_page_header.as_ref().unwrap();
                let is_sorted = dict_header.is_sorted.unwrap_or(false);

                let physical = match reader.descriptor.type_() {
                    ParquetType::PrimitiveType { physical_type, .. } => physical_type,
                    _ => unreachable!(),
                };

                let page = read_page_dict(
                    buffer,
                    dict_header.num_values as u32,
                    (
                        reader.compression,
                        page_header.uncompressed_page_size as usize,
                    ),
                    is_sorted,
                    *physical,
                )?;

                reader.current_dictionary = Some(page);
                continue;
            }
            PageType::DataPage => {
                let header = page_header.data_page_header.unwrap();
                reader.seen_num_values += header.num_values as i64;
                Page::V1(PageV1::new(
                    buffer,
                    header.num_values as u32,
                    header.encoding,
                    (
                        reader.compression,
                        page_header.uncompressed_page_size as usize,
                    ),
                    header.definition_level_encoding,
                    header.repetition_level_encoding,
                    reader.current_dictionary.clone(),
                ))
            }
            PageType::DataPageV2 => {
                let header = page_header.data_page_header_v2.unwrap();
                reader.seen_num_values += header.num_values as i64;
                Page::V2(PageV2::new(
                    buffer,
                    header,
                    (
                        reader.compression,
                        page_header.uncompressed_page_size as usize,
                    ),
                    reader.current_dictionary.clone(),
                ))
            }
            PageType::IndexPage => {
                continue;
            }
        };
        return Ok(Some(result));
    }
    Ok(None)
}

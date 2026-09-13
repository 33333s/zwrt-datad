#include "neighbor.h"
#include <assert.h>
#include <stdio.h>
#include <string.h>
static size_t frame(unsigned char *out) {
    unsigned char raw[34]={0x9d}; raw[4]=0x17;
    uint32_t values[]={3640546464U,0,222,(uint32_t)-920,0};
    for (int i=0;i<5;i++) for (int j=0;j<4;j++) raw[12+i*4+j]=(unsigned char)(values[i]>>(j*8));
    unsigned crc=0xffff;
    for (int i=0;i<32;i++) { crc^=raw[i]; for(int j=0;j<8;j++) crc=(crc>>1)^((crc&1)?0x8408:0); }
    crc^=0xffff;raw[32]=crc;raw[33]=crc>>8;
    size_t n=0;
    for(int i=0;i<34;i++) { if(raw[i]==0x7d || raw[i]==0x7e) { out[n++]=0x7d;out[n++]=raw[i]^0x20; } else out[n++]=raw[i]; }
    out[n++]=0x7e;return n;
}
int main(void) {
    unsigned char data[80];size_t n=frame(data);struct neighbor_result r;
    struct neighbor_parser *p=neighbor_parser_new();assert(p);
    for(size_t i=0;i<n-1;i++) neighbor_parser_feed(p,data+i,1,1,1000);
    neighbor_parser_result(p,1000,&r);assert(!r.count && !r.malformed);
    neighbor_parser_feed(p,data+n-1,1,1,1000);
    neighbor_parser_result(p,1000,&r);assert(r.count==1 && r.cells[0].arfcn==0 && r.cells[0].evidence==NEIGHBOR_EXPLICIT);
    assert(r.cells[0].rsrp_dbm==-92 && r.cells[0].samples==1);
    neighbor_parser_result(p,60000,&r);assert(r.count==1 && r.seen_ms==1000);
    neighbor_parser_result(p,61001,&r);assert(r.count==0 && r.seen_ms==1000);
    neighbor_parser_free(p);
    p=neighbor_parser_new();assert(p);
    for(int i=0;i<4096;i++) neighbor_parser_feed(p,data,n,1,1000);
    neighbor_parser_feed(p,data,n,1,62000);neighbor_parser_result(p,62000,&r);
    assert(r.count==1 && r.cells[0].samples==1 && !r.discarded && !r.partial);
    neighbor_parser_free(p);
    p=neighbor_parser_new();assert(p);
    for(int i=0;i<4097;i++) neighbor_parser_feed(p,data,n,1,1000);
    neighbor_parser_result(p,1000,&r);assert(r.discarded==1 && r.partial && r.cells[0].samples==4096);
    neighbor_parser_free(p);
    p=neighbor_parser_new();assert(p);
    unsigned char malformed[]={1,0x7e};
    neighbor_parser_feed(p,malformed,sizeof malformed,1,1000);
    neighbor_parser_result(p,1000,&r);assert(r.malformed==1 && r.partial);
    neighbor_parser_feed(p,data,n,1,61001);
    neighbor_parser_result(p,61001,&r);
    assert(r.malformed==1 && !r.partial && r.count==1 && r.cells[0].samples==1);
    neighbor_parser_free(p);
    puts("neighbor stream: byte boundaries, observation freshness, TTL, EARFCN zero, bounded records and recent partial status PASS");
    return 0;
}

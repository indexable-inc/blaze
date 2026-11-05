# Comparison of Prost and rust-protobuf for GRPC

Note that these are intermediate results. The implementation of Prost in all our code is incomplete.

## Generated code

kvproto repo: 11 proto files, 2075 LoC

as generated:

rust-protobuf: 2.4mb, 69kloc
prost: 1.0mb, 32kloc

expanded:

rust-protobuf: 4.1mb, 93kloc
prost: 3.5mb, 86kloc

## compilation time

(excluding deps; compiling but not generating Rust code)

debug

protobuf: 7.0s
protobuf + prost: 12.7s (can't build just Prost because of wrappers)

release

protobuf: 6.8s
protobuf + prost: 11.9s

## Benchmarks

On 'high performance CPU' digital ocean droplet, 32GB RAM.

`run_performance_tests.py -l rust`

### Conclusion

Prost was marginally faster than rust-protobuf: average 2.6% lower latency and 2.7% higher QPS. (This average excludes the generic and constrained scenarios). However, these average measures are only slightly larger than the percentage changes between runs with the same codec (1.7% and 1.9%). So I don't think we can say that Prost is faster than rust-protobuf in the GRPC benchmark, but it is certainly not worse. Note that ser/de is not a large part of the GRPC benchmark, so it is likely there will be a more pronounced effect in TiKV.

### Prost

|Scenario                                        |median latency (us) |QPS      |
|------------------------------------------------|-------------------:|--------:|
|rust_generic_async_streaming_ping_pong          |67.4                |14462.1  |
|rust_protobuf_async_streaming_ping_pong         |69.8                |13934.6  |
|rust_protobuf_async_unary_ping_pong             |112.3               |8845.0   |
|rust_protobuf_sync_to_async_unary_ping_pong     |122.3               |8135.4   |
|rust_protobuf_async_unary_qps_unconstrained     |45534.3             |134950.5 |
|rust_protobuf_async_streaming_qps_unconstrained |28036.1             |218596.9 |
|rust_protobuf_async_unary_ping_pong_1MB         |6341.6              |157.0    |


### Protobuf

|Scenario                                        |median latency (us) |QPS      |
|------------------------------------------------|-------------------:|--------:|
|rust_generic_async_streaming_ping_pong          |67.0                |14535.4  |
|rust_protobuf_async_streaming_ping_pong         |70.8                |13696.5  |
|rust_protobuf_async_unary_ping_pong             |115.3               |8616.5   |
|rust_protobuf_sync_to_async_unary_ping_pong     |124.9               |7949.2   |
|rust_protobuf_async_unary_qps_unconstrained     |47557.0             |129103.9 |
|rust_protobuf_async_streaming_qps_unconstrained |28872.4             |213534.3 |
|rust_protobuf_async_unary_ping_pong_1MB         |5808.5              |171.9    |


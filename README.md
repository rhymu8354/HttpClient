# HttpClient (rhymuweb-client)

This is a library which can be used to fetch web resources using Hypertext
Transfer Protocol (HTTP).

[![Crates.io](https://img.shields.io/crates/v/rhymuweb-client.svg)](https://crates.io/crates/rhymuweb-client)
[![Documentation](https://docs.rs/rhymuweb-client/badge.svg)][dox]

More information about the Rust implementation of this library can be found in
the [crate documentation][dox].

[dox]: https://docs.rs/rhymuweb-client

## Usage

The [`HttpClient`] type operates as an HTTP user agent as described in [IETF
RFC 7230](https://tools.ietf.org/html/rfc7230).  It maintains set of
connections to web servers and resource transactions in progress.  A
transaction associates requests with responses, and is created by submitting an
HTTP request (represented by a
[`rhymuweb::Request`](https://docs.rs/rhymuweb/1.0.0/rhymuweb/struct.Request.html))
via
[`HttpClient::request`](https://docs.rs/rhymuweb-client/1.0.0/rhymuweb-client/struct.HttpClient.html#method.request),
which returns a future that provides the response (if any) to the request once
it's completed.

Along with the library is an example program, `rover`, which demonstrates how
to use [`HttpClient`], "fetching" basic web resources and dumping them to
standard output.

[`HttpClient`]: https://docs.rs/rhymuweb-client/1.0.0/rhymuweb-client/struct.HttpClient.html

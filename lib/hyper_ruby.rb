# frozen_string_literal: true

require_relative "hyper_ruby/version"
require_relative "hyper_ruby/hyper_ruby"

# Server#configure takes a hash; alongside the HTTP keys (bind_address,
# tokio_threads, debug, recv_timeout, send_timeout, channel_capacity,
# max_connection_age) the syslog listeners accept:
#
#   syslog_stream_path                 Unix socket path for the stream listener
#   syslog_stream_bind                 "host:port" for the stream listener
#   syslog_udp_bind                    "host:port" for the datagram listener (SO_REUSEPORT)
#   syslog_proxy_protocol              require a PROXY v2 header per connection (default false)
#   syslog_proxy_header_timeout        milliseconds allowed for that header (default 5000)
#   syslog_max_frame_bytes             largest stream frame (default 102400)
#   syslog_max_pending                 undelivered messages allowed across all listeners
#                                      (default 1000)
#   syslog_max_pending_per_connection  undelivered messages allowed for one connection, so
#                                      a stalled sender cannot take the lot (default 64)
#   syslog_max_connections             stream connections accepted at once (default 10000)
#   syslog_idle_timeout_ms             close a connection after this long without data
#                                      (default 0, no timeout)
#   syslog_udp_max_datagram_bytes      largest datagram accepted; one that fills the buffer
#                                      counts as truncated and is dropped (default 65536)
#   syslog_udp_so_rcvbuf               SO_RCVBUF for the datagram socket
#   syslog_work_ratio                  syslog messages a worker may take ahead of a waiting
#                                      request (default 4)
#   syslog_handler                     callable invoked with (message, peer_ip, transport,
#                                      received_at_nanoseconds, message_id, attempt)
#
# The handler is configured rather than passed to Server#run_worker because a
# worker's block already serves HTTP requests, and both kinds of work share those
# threads. transport is :stream or :datagram; peer_ip is nil when the transport
# names no peer, such as a Unix socket connection with no PROXY header. message_id
# is stable across the retries of one message and unique for the life of the
# process, and attempt starts at 1, so a handler can make its retries idempotent.
#
# A truthy return means the message was admitted; anything else, or a raised
# exception, means it was refused: a stream connection holds the message and
# retries it while its reads stall, a datagram is dropped and counted.
#
# Server#syslog_listening? reports listener readiness, and Server#syslog_stats
# returns the transport counters.
module HyperRuby
  class Error < StandardError; end
end

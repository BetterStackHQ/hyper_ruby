# frozen_string_literal: true

require_relative "hyper_ruby/version"
require_relative "hyper_ruby/hyper_ruby"

# Server#configure takes a hash; alongside the HTTP keys (bind_address,
# tokio_threads, debug, recv_timeout, send_timeout, channel_capacity,
# max_connection_age) the syslog listeners accept:
#
#   syslog_stream_path                  Unix socket path for the stream listener
#   syslog_stream_bind                  "host:port" for the stream listener
#   syslog_udp_bind                     "host:port" for the datagram listener (SO_REUSEPORT)
#   syslog_proxy_protocol               require a PROXY v2 header per connection (default false)
#   syslog_proxy_header_timeout         milliseconds allowed for that header (default 5000)
#   syslog_max_frame_bytes              largest stream frame (default 102400)
#   syslog_max_pending                  undelivered messages allowed per connection, and
#                                       across the listeners (default 1000)
#   syslog_udp_recv_buffer_bytes        receive buffer per datagram; a datagram that fills
#                                       it counts as truncated and is dropped (default 65536)
#   syslog_udp_socket_recv_buffer_bytes SO_RCVBUF for the datagram socket
#   syslog_handler                      callable invoked with
#                                       (message, peer_ip, :tcp/:udp, received_at_nanoseconds)
#
# The handler runs on the same worker threads as HTTP requests (Server#run_worker).
# A truthy return means the message was admitted; anything else, or a raised
# exception, means it was refused: a stream connection holds the message and
# retries it while its reads stall, a datagram is dropped and counted.
#
# Server#syslog_listening? reports listener readiness, and Server#syslog_stats
# returns the transport counters.
module HyperRuby
  class Error < StandardError; end
end

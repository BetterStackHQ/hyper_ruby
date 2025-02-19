#!/usr/bin/env ruby
# frozen_string_literal: true

$LOAD_PATH.unshift File.expand_path("../lib", __dir__)

puts "Loading hyper_ruby"

require "hyper_ruby"
require "json"

# Create and configure the server
server = HyperRuby::Server.new
config = {
  bind_address: ENV.fetch("BIND_ADDRESS", "127.0.0.1:3000"),
  tokio_threads: ENV.fetch("TOKIO_THREADS", "1").to_i,
  debug: ENV.fetch("DEBUG", "false") == "true",
  recv_timeout: ENV.fetch("RECV_TIMEOUT", "30000").to_i
}
server.configure(config)

puts "Starting server with config: #{config}"

# Start the server
server.start

puts "Server started"

# Create a worker thread to handle requests
worker = Thread.new do
  server.run_worker do |request|
    buffer = String.new(capacity: 1024)
    request.fill_body(buffer)

    # Create a response that echoes back request details
    response_data = {
      method: request.http_method,
      path: request.path,
      headers: request.headers,
      body: buffer
    }

    HyperRuby::Response.new(
      200,
      { "Content-Type" => "application/json" },
      JSON.pretty_generate(response_data)
    )
  end
end

puts "Server running at #{config[:bind_address]}"
puts "Press Ctrl+C to stop"

# Wait for Ctrl+C
begin
  sleep
rescue Interrupt
  puts "\nShutting down..."
  server.stop
  worker.join
end 
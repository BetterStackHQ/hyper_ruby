# frozen_string_literal: true

require "test_helper"

class TestHyperRuby < Minitest::Test
  def test_that_it_has_a_version_number
    refute_nil ::HyperRuby::VERSION
  end

  def test_it_does_something_useful
    server = HyperRuby::Server.new
    server.configure({ bind_address: "127.0.0.1:3010" })
    handler = Handler.new
    server.start

    # Create ruby worker threads that process requests;
    # 1 is usually enough, and generally handles better than multiple threads
    threads = 1.times.map do
      Thread.new do
        server.run_worker do |request|
          # Process the request in Ruby
          # request is a hash with :method, :path, :headers, and :body keys
          #puts "Processing #{request[:method]} request to #{request[:path]}"
          
          # Return response string
          HyperRuby::Response.new(200, { 'Content-Type' => 'application/json' }, '{ "message": "Hello from Ruby worker!" }')
        end
      end
    end

    gets
    server.stop
  end

  class Handler
    def call(req_info)
      "Hello, world!"
    end
  end
end

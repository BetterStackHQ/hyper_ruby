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
    server.start(handler)
    gets
    server.stop
  end

  class Handler
    def call(req_info)
      "Hello, world!"
    end
  end
end

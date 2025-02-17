# frozen_string_literal: true

require "test_helper"
require "httpx"

class TestHyperRuby < Minitest::Test

  def test_can_read_properties_back
    response = HyperRuby::Response.new(200, { 'Content-Type' => 'text/plain' }, 'Hello, world!')
    assert_equal 200, response.status
    assert_equal 'text/plain', response.headers['content-type']
    assert_equal 'Hello, world!', response.body
  end

end

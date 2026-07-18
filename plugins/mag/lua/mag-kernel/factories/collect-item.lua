local M = {}
M.declaration = { name="collect-item", type_variables={"T"},
  semantic={input={kind="variable",name="T"},output={kind="variable",name="T"},
    inputs={{wire="nefor.structured.Validated",type={kind="variable",name="T"}},
      {wire="nefor.agent.Result",type={kind="variable",name="T"}}},
    outputs={{wire="nefor.dynamic.Item",type={kind="variable",name="T"}}}}, params={},
  inputs={value={"nefor.structured.Validated","nefor.agent.Result"}}, outputs={"nefor.dynamic.Item"}, signals={} }
function M.construct(id, params, emit)
  local instance={id=id}
  function instance.deliver(a)
    emit({kind="nefor.dynamic.Item",from=id,value=a.messages[1].message.value})
    return {status="ok"}
  end
  emit({kind="mag.ready",from=id}); return instance
end
return M

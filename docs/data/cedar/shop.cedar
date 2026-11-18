// The example shop that jenny-rest's end-to-end authorization test runs against.

@id("admin")
permit (principal in Role::"Admin", action, resource in Adapter::"urn:shop");

// Dispatch reads orders, drives their status and may look a payment up.
@id("dispatch-reads-orders")
permit (
  principal in Role::"Dispatch",
  action in Action::"read",
  resource in Resource::"urn:shop:orders"
);

@id("dispatch-drives-status")
permit (
  principal in Role::"Dispatch",
  action in [Action::"POST", Action::"PUT", Action::"PATCH"],
  resource == Route::"/api/orders/{id}/status"
);

@id("dispatch-reads-payments")
permit (
  principal in Role::"Dispatch",
  action in Action::"read",
  resource == Route::"/api/payments/{id}"
);

// Purchasing owns the catalogue.
@id("purchasing")
permit (
  principal in Role::"Purchasing",
  action,
  resource in Resource::"urn:shop:products"
);

// Customer support is a member of Dispatch (see shop.entities.json) and also handles
// payments, cancellations and customers.
@id("support-amends-payments")
permit (
  principal in Role::"CustomerSupport",
  action in [Action::"POST", Action::"PUT", Action::"PATCH"],
  resource == Route::"/api/payments/{id}"
);

@id("support-cancels-orders")
permit (
  principal in Role::"CustomerSupport",
  action == Action::"DELETE",
  resource == Route::"/api/orders/{id}"
);

@id("support-manages-customers")
permit (
  principal in Role::"CustomerSupport",
  action,
  resource == Route::"/api/customers/{id}"
);

@id("support-lists-customers")
permit (
  principal in Role::"CustomerSupport",
  action in Action::"read",
  resource == Route::"/api/customers"
);

// Customers sign up, place orders and pay, browse the catalogue and keep their account.
@id("customer-creates")
permit (principal in Role::"Customer", action == Action::"POST", resource)
when {
  resource in
    [Route::"/api/orders", Route::"/api/payments", Route::"/api/customers"]
};

@id("customer-browses")
permit (
  principal in Role::"Customer",
  action in Action::"read",
  resource in Resource::"urn:shop:products"
);

@id("customer-account")
permit (
  principal in Role::"Customer",
  action in [Action::"GET", Action::"POST", Action::"PUT", Action::"PATCH"],
  resource == Route::"/api/customers/{id}/account"
);

// Nobody suspended does anything, whatever else they hold.
@id("suspended")
forbid (principal in Role::"Suspended", action, resource);

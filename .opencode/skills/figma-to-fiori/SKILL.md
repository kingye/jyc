---
name: figma-to-fiori
description: Generate SAP CAP Node.js backend and Fiori UI5 frontend code from Figma designs. Supports both Figma links and exported design images.
origin: custom
---

# Figma to SAP CAP + Fiori UI5 Generator

Transform Figma designs into complete SAP application code with CAP backend services and Fiori UI5 frontend.

## When to Activate

- User provides a Figma link or exported design image
- User requests CAP + UI5 code generation from visual designs
- Building Fiori Elements or freestyle UI5 applications
- Creating SAP BTP applications with visual mockups

## Input Methods

### 1. Figma Link
```
User: Generate code from https://www.figma.com/file/xxx/Design-Name
```

### 2. Exported Image
```
User: [attaches screenshot/exported PNG]
Generate CAP and UI5 code from this design
```

## Analysis Process

When receiving a design, analyze:

1. **UI Components** - Identify tables, forms, lists, cards, charts, buttons, inputs
2. **Data Model** - Infer entities, properties, relationships from displayed data
3. **Actions** - Identify CRUD operations, custom actions, navigation patterns
4. **Layout** - Determine page structure, responsive behavior, sections

## Code Generation Output

### 1. CAP Backend Structure

```
srv/
├── schema.cds          # Data model definitions
├── service.cds         # Service exposures
├── service.js          # Custom handlers (if needed)
└── annotations.cds     # UI annotations
db/
├── data/               # Sample CSV data
└── schema.cds          # Database schema
```

### 2. UI5 Frontend Structure

```
app/
├── webapp/
│   ├── manifest.json       # App descriptor
│   ├── Component.js        # UI5 Component
│   ├── index.html          # Entry point
│   ├── view/
│   │   ├── App.view.xml    # Root view
│   │   └── Main.view.xml   # Main content
│   ├── controller/
│   │   ├── App.controller.js
│   │   └── Main.controller.js
│   ├── model/
│   │   └── formatter.js    # Formatters
│   ├── i18n/
│   │   └── i18n.properties # Translations
│   └── css/
│       └── style.css       # Custom styles
└── package.json
```

## CAP Code Patterns

### Data Model (db/schema.cds)

```cds
namespace my.app;

using { cuid, managed } from '@sap/cds/common';

entity Products : cuid, managed {
  name        : String(100) @mandatory;
  description : String(1000);
  price       : Decimal(10,2);
  currency    : String(3);
  category    : Association to Categories;
  stock       : Integer default 0;
  status      : String enum { active; inactive; discontinued };
}

entity Categories : cuid {
  name     : String(50);
  products : Association to many Products on products.category = $self;
}
```

### Service Definition (srv/service.cds)

```cds
using my.app from '../db/schema';

service CatalogService @(path: '/catalog') {
  
  @odata.draft.enabled
  entity Products as projection on app.Products {
    *,
    category.name as categoryName
  } actions {
    action activate() returns Products;
    action deactivate() returns Products;
  };
  
  @readonly
  entity Categories as projection on app.Categories;
  
  // Value helps
  @cds.odata.valuelist
  entity VH_Categories as projection on app.Categories {
    key ID,
    name
  };
}
```

### UI Annotations (srv/annotations.cds)

```cds
using CatalogService from './service';

// List Report annotations
annotate CatalogService.Products with @(
  UI: {
    HeaderInfo: {
      TypeName: 'Product',
      TypeNamePlural: 'Products',
      Title: { Value: name },
      Description: { Value: description }
    },
    
    SelectionFields: [ name, category_ID, status ],
    
    LineItem: [
      { Value: name, Label: 'Name' },
      { Value: categoryName, Label: 'Category' },
      { Value: price, Label: 'Price' },
      { Value: stock, Label: 'Stock' },
      { Value: status, Label: 'Status', Criticality: statusCriticality }
    ],
    
    Facets: [
      {
        $Type: 'UI.ReferenceFacet',
        Target: '@UI.FieldGroup#General',
        Label: 'General Information'
      },
      {
        $Type: 'UI.ReferenceFacet',
        Target: '@UI.FieldGroup#Pricing',
        Label: 'Pricing & Stock'
      }
    ],
    
    FieldGroup#General: {
      Data: [
        { Value: name },
        { Value: description },
        { Value: category_ID },
        { Value: status }
      ]
    },
    
    FieldGroup#Pricing: {
      Data: [
        { Value: price },
        { Value: currency },
        { Value: stock }
      ]
    }
  }
);

// Value Help annotations
annotate CatalogService.Products with {
  category @(
    Common: {
      Text: category.name,
      TextArrangement: #TextOnly,
      ValueList: {
        CollectionPath: 'VH_Categories',
        Parameters: [
          { $Type: 'Common.ValueListParameterInOut', LocalDataProperty: category_ID, ValueListProperty: 'ID' },
          { $Type: 'Common.ValueListParameterDisplayOnly', ValueListProperty: 'name' }
        ]
      }
    }
  );
};
```

### Service Implementation (srv/service.js)

```javascript
const cds = require('@sap/cds');

module.exports = class CatalogService extends cds.ApplicationService {
  
  async init() {
    const { Products } = this.entities;
    
    // Before create/update validation
    this.before(['CREATE', 'UPDATE'], Products, async (req) => {
      const { price, stock } = req.data;
      
      if (price !== undefined && price < 0) {
        req.error(400, 'Price cannot be negative');
      }
      
      if (stock !== undefined && stock < 0) {
        req.error(400, 'Stock cannot be negative');
      }
    });
    
    // Custom action: activate
    this.on('activate', Products, async (req) => {
      const { ID } = req.params[0];
      await UPDATE(Products).set({ status: 'active' }).where({ ID });
      return SELECT.one.from(Products).where({ ID });
    });
    
    // Custom action: deactivate
    this.on('deactivate', Products, async (req) => {
      const { ID } = req.params[0];
      await UPDATE(Products).set({ status: 'inactive' }).where({ ID });
      return SELECT.one.from(Products).where({ ID });
    });
    
    await super.init();
  }
};
```

## UI5 Code Patterns

### manifest.json

```json
{
  "_version": "1.59.0",
  "sap.app": {
    "id": "my.app",
    "type": "application",
    "title": "{{appTitle}}",
    "description": "{{appDescription}}",
    "applicationVersion": { "version": "1.0.0" },
    "dataSources": {
      "mainService": {
        "uri": "/catalog/",
        "type": "OData",
        "settings": {
          "odataVersion": "4.0",
          "localUri": "localService/metadata.xml"
        }
      }
    }
  },
  "sap.ui": {
    "technology": "UI5",
    "icons": {
      "icon": "sap-icon://product"
    },
    "deviceTypes": {
      "desktop": true,
      "tablet": true,
      "phone": true
    }
  },
  "sap.ui5": {
    "flexEnabled": true,
    "dependencies": {
      "minUI5Version": "1.120.0",
      "libs": {
        "sap.m": {},
        "sap.ui.core": {},
        "sap.f": {},
        "sap.ui.layout": {}
      }
    },
    "contentDensities": {
      "compact": true,
      "cozy": true
    },
    "models": {
      "i18n": {
        "type": "sap.ui.model.resource.ResourceModel",
        "settings": {
          "bundleName": "my.app.i18n.i18n"
        }
      },
      "": {
        "dataSource": "mainService",
        "preload": true,
        "settings": {
          "operationMode": "Server",
          "autoExpandSelect": true,
          "earlyRequests": true
        }
      }
    },
    "routing": {
      "config": {
        "routerClass": "sap.m.routing.Router",
        "viewType": "XML",
        "viewPath": "my.app.view",
        "controlId": "app",
        "controlAggregation": "pages",
        "async": true
      },
      "routes": [
        {
          "name": "main",
          "pattern": "",
          "target": "main"
        },
        {
          "name": "detail",
          "pattern": "Products({key})",
          "target": "detail"
        }
      ],
      "targets": {
        "main": { "viewName": "Main", "viewLevel": 1 },
        "detail": { "viewName": "Detail", "viewLevel": 2 }
      }
    }
  }
}
```

### Main.view.xml (List with Table)

```xml
<mvc:View
  controllerName="my.app.controller.Main"
  xmlns="sap.m"
  xmlns:mvc="sap.ui.core.mvc"
  xmlns:f="sap.f"
  xmlns:core="sap.ui.core">
  
  <f:DynamicPage id="dynamicPage" headerExpanded="true">
    <f:title>
      <f:DynamicPageTitle>
        <f:heading>
          <Title text="{i18n>productsTitle}"/>
        </f:heading>
        <f:actions>
          <Button
            text="{i18n>createBtn}"
            type="Emphasized"
            press=".onCreatePress"/>
          <Button
            text="{i18n>refreshBtn}"
            icon="sap-icon://refresh"
            press=".onRefreshPress"/>
        </f:actions>
      </f:DynamicPageTitle>
    </f:title>
    
    <f:header>
      <f:DynamicPageHeader>
        <f:content>
          <FlexBox wrap="Wrap">
            <VBox class="sapUiSmallMarginEnd">
              <Label text="{i18n>filterName}"/>
              <SearchField
                id="searchField"
                width="300px"
                search=".onSearch"/>
            </VBox>
            <VBox class="sapUiSmallMarginEnd">
              <Label text="{i18n>filterCategory}"/>
              <ComboBox
                id="categoryFilter"
                items="{/Categories}"
                selectionChange=".onFilterChange">
                <core:Item key="{ID}" text="{name}"/>
              </ComboBox>
            </VBox>
          </FlexBox>
        </f:content>
      </f:DynamicPageHeader>
    </f:header>
    
    <f:content>
      <Table
        id="productsTable"
        items="{
          path: '/Products',
          sorter: { path: 'name' }
        }"
        growing="true"
        growingThreshold="20"
        mode="SingleSelectMaster"
        selectionChange=".onSelectionChange">
        
        <headerToolbar>
          <OverflowToolbar>
            <Title text="{i18n>productCount} ({= ${/Products}.length})"/>
            <ToolbarSpacer/>
            <Button
              icon="sap-icon://excel-attachment"
              tooltip="{i18n>exportExcel}"
              press=".onExportPress"/>
          </OverflowToolbar>
        </headerToolbar>
        
        <columns>
          <Column><Text text="{i18n>colName}"/></Column>
          <Column><Text text="{i18n>colCategory}"/></Column>
          <Column hAlign="End"><Text text="{i18n>colPrice}"/></Column>
          <Column hAlign="End"><Text text="{i18n>colStock}"/></Column>
          <Column><Text text="{i18n>colStatus}"/></Column>
        </columns>
        
        <items>
          <ColumnListItem type="Navigation" press=".onItemPress">
            <cells>
              <ObjectIdentifier title="{name}" text="{description}"/>
              <Text text="{category/name}"/>
              <ObjectNumber
                number="{
                  path: 'price',
                  type: 'sap.ui.model.type.Currency',
                  formatOptions: { showMeasure: false }
                }"
                unit="{currency}"/>
              <ObjectNumber number="{stock}" state="{= ${stock} > 10 ? 'Success' : 'Warning'}"/>
              <ObjectStatus
                text="{status}"
                state="{= ${status} === 'active' ? 'Success' : 'Error'}"/>
            </cells>
          </ColumnListItem>
        </items>
      </Table>
    </f:content>
  </f:DynamicPage>
</mvc:View>
```

### Main.controller.js

```javascript
sap.ui.define([
  "sap/ui/core/mvc/Controller",
  "sap/ui/model/Filter",
  "sap/ui/model/FilterOperator",
  "sap/m/MessageBox",
  "sap/m/MessageToast"
], function(Controller, Filter, FilterOperator, MessageBox, MessageToast) {
  "use strict";
  
  return Controller.extend("my.app.controller.Main", {
    
    onInit: function() {
      this._oTable = this.byId("productsTable");
    },
    
    onSearch: function(oEvent) {
      const sQuery = oEvent.getParameter("query");
      const aFilters = [];
      
      if (sQuery) {
        aFilters.push(new Filter({
          filters: [
            new Filter("name", FilterOperator.Contains, sQuery),
            new Filter("description", FilterOperator.Contains, sQuery)
          ],
          and: false
        }));
      }
      
      this._applyFilters(aFilters);
    },
    
    onFilterChange: function(oEvent) {
      const oComboBox = this.byId("categoryFilter");
      const sKey = oComboBox.getSelectedKey();
      const aFilters = [];
      
      if (sKey) {
        aFilters.push(new Filter("category_ID", FilterOperator.EQ, sKey));
      }
      
      this._applyFilters(aFilters);
    },
    
    _applyFilters: function(aFilters) {
      const oBinding = this._oTable.getBinding("items");
      oBinding.filter(aFilters);
    },
    
    onCreatePress: function() {
      this.getOwnerComponent().getRouter().navTo("detail", {
        key: "new"
      });
    },
    
    onItemPress: function(oEvent) {
      const oItem = oEvent.getSource();
      const oContext = oItem.getBindingContext();
      const sID = oContext.getProperty("ID");
      
      this.getOwnerComponent().getRouter().navTo("detail", {
        key: sID
      });
    },
    
    onRefreshPress: function() {
      this._oTable.getBinding("items").refresh();
      MessageToast.show(this._getText("refreshed"));
    },
    
    onExportPress: function() {
      // Export to Excel implementation
      sap.ui.require([
        "sap/ui/export/Spreadsheet"
      ], function(Spreadsheet) {
        const oTable = this._oTable;
        const oBinding = oTable.getBinding("items");
        
        const oSettings = {
          workbook: { columns: [
            { label: "Name", property: "name" },
            { label: "Category", property: "category/name" },
            { label: "Price", property: "price", type: "Number" },
            { label: "Stock", property: "stock", type: "Number" },
            { label: "Status", property: "status" }
          ]},
          dataSource: {
            type: "odata",
            dataUrl: oBinding.getDownloadUrl(),
            serviceUrl: this.getOwnerComponent().getModel().getServiceUrl()
          },
          fileName: "Products.xlsx"
        };
        
        new Spreadsheet(oSettings).build();
      }.bind(this));
    },
    
    _getText: function(sKey, aArgs) {
      return this.getOwnerComponent().getModel("i18n")
        .getResourceBundle().getText(sKey, aArgs);
    }
  });
});
```

### Detail.view.xml (Form View)

```xml
<mvc:View
  controllerName="my.app.controller.Detail"
  xmlns="sap.m"
  xmlns:mvc="sap.ui.core.mvc"
  xmlns:f="sap.f"
  xmlns:form="sap.ui.layout.form"
  xmlns:core="sap.ui.core">
  
  <f:DynamicPage id="detailPage">
    <f:title>
      <f:DynamicPageTitle>
        <f:heading>
          <Title text="{name}"/>
        </f:heading>
        <f:snappedContent>
          <ObjectStatus text="{status}" state="{= ${status} === 'active' ? 'Success' : 'Error'}"/>
        </f:snappedContent>
        <f:actions>
          <Button
            text="{i18n>saveBtn}"
            type="Emphasized"
            press=".onSavePress"
            visible="{= ${viewModel>/editMode}}"/>
          <Button
            text="{i18n>editBtn}"
            press=".onEditPress"
            visible="{= !${viewModel>/editMode}}"/>
          <Button
            text="{i18n>cancelBtn}"
            press=".onCancelPress"
            visible="{= ${viewModel>/editMode}}"/>
          <Button
            text="{i18n>deleteBtn}"
            type="Reject"
            press=".onDeletePress"
            visible="{= !${viewModel>/editMode} &amp;&amp; !${viewModel>/createMode}}"/>
        </f:actions>
      </f:DynamicPageTitle>
    </f:title>
    
    <f:content>
      <VBox class="sapUiResponsiveMargin">
        <form:SimpleForm
          editable="{viewModel>/editMode}"
          layout="ResponsiveGridLayout"
          labelSpanXL="4" labelSpanL="4" labelSpanM="4" labelSpanS="12"
          emptySpanXL="0" emptySpanL="0" emptySpanM="0" emptySpanS="0"
          columnsXL="2" columnsL="2" columnsM="2" columnsS="1">
          
          <form:toolbar>
            <Toolbar>
              <Title text="{i18n>generalInfo}"/>
            </Toolbar>
          </form:toolbar>
          
          <Label text="{i18n>name}" required="true"/>
          <Input value="{name}" maxLength="100"/>
          
          <Label text="{i18n>description}"/>
          <TextArea value="{description}" rows="3"/>
          
          <Label text="{i18n>category}"/>
          <ComboBox
            selectedKey="{category_ID}"
            items="{/Categories}">
            <core:Item key="{ID}" text="{name}"/>
          </ComboBox>
          
          <Label text="{i18n>status}"/>
          <Select selectedKey="{status}">
            <core:Item key="active" text="{i18n>statusActive}"/>
            <core:Item key="inactive" text="{i18n>statusInactive}"/>
            <core:Item key="discontinued" text="{i18n>statusDiscontinued}"/>
          </Select>
          
          <form:toolbar>
            <Toolbar>
              <Title text="{i18n>pricingStock}"/>
            </Toolbar>
          </form:toolbar>
          
          <Label text="{i18n>price}" required="true"/>
          <Input value="{price}" type="Number"/>
          
          <Label text="{i18n>currency}"/>
          <ComboBox selectedKey="{currency}">
            <core:Item key="USD" text="USD"/>
            <core:Item key="EUR" text="EUR"/>
            <core:Item key="CNY" text="CNY"/>
          </ComboBox>
          
          <Label text="{i18n>stock}"/>
          <Input value="{stock}" type="Number"/>
          
        </form:SimpleForm>
      </VBox>
    </f:content>
    
    <f:footer>
      <OverflowToolbar visible="{= ${viewModel>/editMode}}">
        <ToolbarSpacer/>
        <Button text="{i18n>saveBtn}" type="Emphasized" press=".onSavePress"/>
        <Button text="{i18n>cancelBtn}" press=".onCancelPress"/>
      </OverflowToolbar>
    </f:footer>
  </f:DynamicPage>
</mvc:View>
```

### Detail.controller.js

```javascript
sap.ui.define([
  "sap/ui/core/mvc/Controller",
  "sap/ui/model/json/JSONModel",
  "sap/m/MessageBox",
  "sap/m/MessageToast"
], function(Controller, JSONModel, MessageBox, MessageToast) {
  "use strict";
  
  return Controller.extend("my.app.controller.Detail", {
    
    onInit: function() {
      const oViewModel = new JSONModel({
        editMode: false,
        createMode: false
      });
      this.getView().setModel(oViewModel, "viewModel");
      
      this.getOwnerComponent().getRouter()
        .getRoute("detail")
        .attachPatternMatched(this._onRouteMatched, this);
    },
    
    _onRouteMatched: function(oEvent) {
      const sKey = oEvent.getParameter("arguments").key;
      const oViewModel = this.getView().getModel("viewModel");
      
      if (sKey === "new") {
        oViewModel.setProperty("/editMode", true);
        oViewModel.setProperty("/createMode", true);
        this._createNewEntry();
      } else {
        oViewModel.setProperty("/editMode", false);
        oViewModel.setProperty("/createMode", false);
        this._bindProduct(sKey);
      }
    },
    
    _bindProduct: function(sID) {
      const oView = this.getView();
      oView.bindElement({
        path: `/Products(${sID})`,
        parameters: {
          $expand: "category"
        }
      });
    },
    
    _createNewEntry: function() {
      const oModel = this.getOwnerComponent().getModel();
      const oListBinding = oModel.bindList("/Products");
      const oContext = oListBinding.create({
        status: "active",
        currency: "USD",
        stock: 0
      });
      
      this.getView().setBindingContext(oContext);
    },
    
    onEditPress: function() {
      this.getView().getModel("viewModel").setProperty("/editMode", true);
    },
    
    onCancelPress: function() {
      const oViewModel = this.getView().getModel("viewModel");
      const bCreateMode = oViewModel.getProperty("/createMode");
      
      if (bCreateMode) {
        this.getView().getBindingContext().delete();
        this.getOwnerComponent().getRouter().navTo("main");
      } else {
        this.getView().getModel().resetChanges();
        oViewModel.setProperty("/editMode", false);
      }
    },
    
    onSavePress: async function() {
      try {
        await this.getView().getModel().submitBatch("updateGroup");
        
        MessageToast.show(this._getText("saved"));
        
        const oViewModel = this.getView().getModel("viewModel");
        oViewModel.setProperty("/editMode", false);
        
        if (oViewModel.getProperty("/createMode")) {
          oViewModel.setProperty("/createMode", false);
          const sID = this.getView().getBindingContext().getProperty("ID");
          this.getOwnerComponent().getRouter().navTo("detail", { key: sID });
        }
      } catch (oError) {
        MessageBox.error(this._getText("saveError"));
      }
    },
    
    onDeletePress: function() {
      MessageBox.confirm(this._getText("deleteConfirm"), {
        onClose: async (sAction) => {
          if (sAction === MessageBox.Action.OK) {
            try {
              await this.getView().getBindingContext().delete();
              MessageToast.show(this._getText("deleted"));
              this.getOwnerComponent().getRouter().navTo("main");
            } catch (oError) {
              MessageBox.error(this._getText("deleteError"));
            }
          }
        }
      });
    },
    
    _getText: function(sKey, aArgs) {
      return this.getOwnerComponent().getModel("i18n")
        .getResourceBundle().getText(sKey, aArgs);
    }
  });
});
```

## Fiori Elements Alternative

For rapid development, prefer Fiori Elements with annotations:

### app/products/webapp/manifest.json (Fiori Elements)

```json
{
  "sap.app": {
    "id": "my.app.products",
    "type": "application",
    "dataSources": {
      "mainService": {
        "uri": "/catalog/",
        "type": "OData",
        "settings": { "odataVersion": "4.0" }
      }
    }
  },
  "sap.ui5": {
    "dependencies": {
      "libs": {
        "sap.fe.templates": {}
      }
    },
    "models": {
      "": { "dataSource": "mainService" }
    },
    "routing": {
      "routes": [
        {
          "pattern": ":?query:",
          "name": "ProductsList",
          "target": "ProductsList"
        },
        {
          "pattern": "Products({key}):?query:",
          "name": "ProductsObjectPage",
          "target": "ProductsObjectPage"
        }
      ],
      "targets": {
        "ProductsList": {
          "type": "Component",
          "id": "ProductsList",
          "name": "sap.fe.templates.ListReport",
          "options": {
            "settings": {
              "entitySet": "Products",
              "navigation": {
                "Products": {
                  "detail": { "route": "ProductsObjectPage" }
                }
              }
            }
          }
        },
        "ProductsObjectPage": {
          "type": "Component",
          "id": "ProductsObjectPage",
          "name": "sap.fe.templates.ObjectPage",
          "options": {
            "settings": {
              "entitySet": "Products",
              "editableHeaderContent": true
            }
          }
        }
      }
    }
  }
}
```

## Component Mapping

| Figma Component | UI5 Control | CAP Annotation |
|-----------------|-------------|----------------|
| Table | sap.m.Table / sap.ui.table.Table | @UI.LineItem |
| Form | sap.ui.layout.form.SimpleForm | @UI.FieldGroup |
| Card | sap.f.Card | @UI.DataPoint |
| Chart | sap.viz.ui5.controls.VizFrame | @UI.Chart |
| List | sap.m.List | @UI.LineItem |
| Search | sap.m.SearchField | @UI.SelectionFields |
| Filter Bar | sap.ui.comp.filterbar.FilterBar | @UI.SelectionFields |
| Button (Primary) | sap.m.Button type="Emphasized" | @UI.Identification |
| Button (Action) | sap.m.Button | @cds.odata.Action |
| Input | sap.m.Input | @UI.FieldGroup |
| Select | sap.m.Select / ComboBox | @Common.ValueList |
| Date Picker | sap.m.DatePicker | type: Date |
| Switch | sap.m.Switch | type: Boolean |
| Object Status | sap.m.ObjectStatus | @UI.Criticality |
| Avatar | sap.m.Avatar | - |
| Icon Tab | sap.m.IconTabBar | @UI.Facets |
| Page Header | sap.f.DynamicPageTitle | @UI.HeaderInfo |

## Generation Workflow

1. **Analyze Design** - Identify all UI components and data relationships
2. **Generate Data Model** - Create CDS entities matching the data shown
3. **Create Service** - Expose entities with appropriate annotations
4. **Generate UI** - Choose Fiori Elements (faster) or Freestyle (more control)
5. **Add Handlers** - Implement custom business logic if needed
6. **Test** - Verify with `cds watch` and browser

## Best Practices

1. **Use Fiori Elements first** - Only go freestyle when annotations can't achieve the design
2. **Follow Fiori Design Guidelines** - Maintain consistency with SAP Fiori patterns
3. **Leverage Annotations** - Let CAP generate UI through annotations where possible
4. **Mobile-first** - Use responsive controls (DynamicPage, SimpleForm with ResponsiveGridLayout)
5. **Draft Support** - Enable for complex forms with `@odata.draft.enabled`
6. **i18n** - Always externalize text to i18n properties files

---

# UX Design Guidelines for SAP Fiori

## Core Design Principles

### 1. Role-Based Design
Design for specific user roles and their tasks:
- **Identify user roles** - Who will use this application?
- **Map key tasks** - What are the 3-5 most common tasks?
- **Prioritize actions** - Most frequent actions should be most accessible

### 2. SAP Fiori Design Pillars

| Pillar | Description | Implementation |
|--------|-------------|----------------|
| **Role-based** | Designed for user's role | Personalized launchpad, role-specific apps |
| **Adaptive** | Works across devices | Responsive layouts, touch-friendly |
| **Coherent** | Consistent experience | Follow Fiori design language |
| **Simple** | Focused on key tasks | Reduce clutter, progressive disclosure |
| **Delightful** | Engaging and efficient | Animations, instant feedback |

## Floorplans

Choose the right floorplan based on the use case:

### List Report + Object Page
**Best for:** Master-detail scenarios, CRUD operations on business objects
```
┌─────────────────────────────────────────┐
│ [Filter Bar                           ] │
├─────────────────────────────────────────┤
│ ☐ Name        │ Category │ Status      │
│ ☐ Product A   │ Cat 1    │ ● Active    │
│ ☐ Product B   │ Cat 2    │ ○ Inactive  │
│ ☐ Product C   │ Cat 1    │ ● Active    │
└─────────────────────────────────────────┘
        │
        ▼ (navigate)
┌─────────────────────────────────────────┐
│ Product A                    [Edit]     │
│ ─────────────────────────────────────── │
│ [General] [Details] [History]           │
│ ┌─────────────────────────────────────┐ │
│ │ Name: Product A                     │ │
│ │ Category: Cat 1                     │ │
│ │ Price: $100.00                      │ │
│ └─────────────────────────────────────┘ │
└─────────────────────────────────────────┘
```

### Worklist
**Best for:** Task-oriented lists with inline actions
```
┌─────────────────────────────────────────┐
│ My Tasks                    [Create]    │
├─────────────────────────────────────────┤
│ ┌───────────────────────────────────┐   │
│ │ ● Task 1          Due: Today      │   │
│ │   Description...   [Complete] [X] │   │
│ └───────────────────────────────────┘   │
│ ┌───────────────────────────────────┐   │
│ │ ○ Task 2          Due: Tomorrow   │   │
│ │   Description...   [Complete] [X] │   │
│ └───────────────────────────────────┘   │
└─────────────────────────────────────────┘
```

### Analytical List Page
**Best for:** Data analysis with KPIs and charts
```
┌─────────────────────────────────────────┐
│ Sales Overview                          │
├───────────┬───────────┬─────────────────┤
│ $1.2M     │ 156       │ +12%            │
│ Revenue   │ Orders    │ Growth          │
├───────────┴───────────┴─────────────────┤
│ [Filter Bar                           ] │
├─────────────────────────────────────────┤
│ ┌─────────────────┐  ┌────────────────┐ │
│ │   📊 Chart      │  │ Table          │ │
│ │                 │  │ ...            │ │
│ └─────────────────┘  └────────────────┘ │
└─────────────────────────────────────────┘
```

### Overview Page
**Best for:** Executive dashboards, quick insights
```
┌─────────────────────────────────────────┐
│ Overview                                │
├──────────────────┬──────────────────────┤
│ ┌──────────────┐ │ ┌──────────────────┐ │
│ │ KPI Card     │ │ │ List Card        │ │
│ │ $1.2M        │ │ │ • Item 1         │ │
│ └──────────────┘ │ │ • Item 2         │ │
│ ┌──────────────┐ │ └──────────────────┘ │
│ │ Chart Card   │ │ ┌──────────────────┐ │
│ │   📊         │ │ │ Table Card       │ │
│ └──────────────┘ │ └──────────────────┘ │
└──────────────────┴──────────────────────┘
```

## Visual Hierarchy

### Semantic Colors
Use colors consistently to convey meaning:

```css
/* Status Colors */
--sapPositiveColor: #107e3e;    /* Success, Active, Approved */
--sapCriticalColor: #e9730c;    /* Warning, Pending, Attention */
--sapNegativeColor: #bb0000;    /* Error, Rejected, Blocked */
--sapNeutralColor: #6a6d70;     /* Inactive, Draft, Neutral */
--sapInformativeColor: #0a6ed1; /* Information, In Progress */
```

### Criticality Mapping (CAP Annotations)
```cds
// In annotations.cds
annotate Products with {
  status @UI.Criticality: statusCriticality;
};

// Computed field in service.cds
entity Products as projection on db.Products {
  *,
  // 1=Negative, 2=Critical, 3=Positive, 0=Neutral
  case status
    when 'active' then 3
    when 'inactive' then 2
    when 'discontinued' then 1
    else 0
  end as statusCriticality: Integer
};
```

### Typography Hierarchy
```
Title 1 (H1)   - Page titles, app headers
Title 2 (H2)   - Section headers, card titles
Title 3 (H3)   - Subsection headers
Body           - Primary content text
Caption        - Secondary info, timestamps, hints
```

## Layout Patterns

### Responsive Grid
```xml
<!-- ResponsiveGridLayout adapts columns based on screen size -->
<form:SimpleForm
  layout="ResponsiveGridLayout"
  labelSpanXL="4" labelSpanL="4" labelSpanM="4" labelSpanS="12"
  emptySpanXL="0" emptySpanL="0" emptySpanM="0" emptySpanS="0"
  columnsXL="2" columnsL="2" columnsM="2" columnsS="1">
```

### Content Density
```javascript
// Auto-detect device and apply appropriate density
const sContentDensity = Device.support.touch ? "sapUiSizeCozy" : "sapUiSizeCompact";
this.getView().addStyleClass(sContentDensity);
```

| Density | Touch Target | Use Case |
|---------|--------------|----------|
| Compact | 2rem (32px) | Desktop, mouse |
| Cozy | 2.75rem (44px) | Touch devices |

### Spacing Scale
```
--sapUiTinyMargin: 0.5rem;   /* 8px - minimal spacing */
--sapUiSmallMargin: 1rem;    /* 16px - between related elements */
--sapUiMediumMargin: 2rem;   /* 32px - between sections */
--sapUiLargeMargin: 3rem;    /* 48px - major separations */
```

## Interaction Patterns

### Navigation
```
┌─────────────────────────────────────────┐
│ Shell Bar (App header, user menu)       │
├─────────────────────────────────────────┤
│ [←] Page Title              [Actions]   │  ← Page header
├─────────────────────────────────────────┤
│                                         │
│            Page Content                 │
│                                         │
└─────────────────────────────────────────┘
```

**Navigation types:**
- **Drill-down**: List → Detail (push navigation)
- **Lateral**: Between peer pages (replace navigation)
- **External**: Open in new tab/dialog

### Action Placement
```
┌─────────────────────────────────────────┐
│ Title                    [Edit] [Save]  │  ← Global actions (header)
├─────────────────────────────────────────┤
│ Section Title            [Add]          │  ← Section actions
│ ┌─────────────────────────────────────┐ │
│ │ Item              [Action1][Action2]│ │  ← Inline actions
│ └─────────────────────────────────────┘ │
├─────────────────────────────────────────┤
│              [Cancel] [Save]            │  ← Footer actions (edit mode)
└─────────────────────────────────────────┘
```

**Button types:**
- `Emphasized` - Primary action (max 1 per page)
- `Default` - Secondary actions
- `Transparent` - Toolbar actions with icons
- `Ghost` - Less prominent actions
- `Accept` - Positive confirmations (green)
- `Reject` - Destructive actions (red)

### Feedback Patterns

```javascript
// Toast for success (auto-dismiss)
MessageToast.show("Item saved successfully");

// Message strip for persistent info
<MessageStrip type="Warning" text="Some fields need attention"/>

// Dialog for confirmations
MessageBox.confirm("Delete this item?", {
  actions: [MessageBox.Action.DELETE, MessageBox.Action.CANCEL],
  emphasizedAction: MessageBox.Action.DELETE,
  onClose: (action) => { ... }
});

// Full-page error for critical failures
<IllustratedMessage
  illustrationType="sapIllus-ErrorScreen"
  title="Something went wrong"
  description="Please try again later"/>
```

## Responsive Design

### Breakpoints
| Size | Width | Columns | Use Case |
|------|-------|---------|----------|
| S | < 600px | 1 | Phone |
| M | 600-1024px | 2 | Tablet portrait |
| L | 1024-1440px | 3 | Tablet landscape, small desktop |
| XL | > 1440px | 4 | Large desktop |

### Adaptive Behavior
```xml
<!-- Hide on small screens -->
<Button visible="{= ${device>/system/phone} === false}"/>

<!-- Different layouts per breakpoint -->
<FlexBox
  direction="{= ${device>/system/phone} ? 'Column' : 'Row'}">
```

### Master-Detail Responsive
```javascript
// FlexibleColumnLayout modes
// OneColumn: Phone (list only or detail only)
// TwoColumnsMidExpanded: Tablet (list + detail)
// ThreeColumnsMidExpanded: Desktop (list + detail + sub-detail)
```

## Accessibility (a11y)

### Required Practices
1. **Labels** - All inputs must have associated labels
2. **ARIA** - Use ariaLabel for icon-only buttons
3. **Keyboard** - All actions reachable via keyboard
4. **Color** - Don't rely solely on color (use icons/text)
5. **Contrast** - Minimum 4.5:1 for text

```xml
<!-- Icon button with aria label -->
<Button
  icon="sap-icon://delete"
  tooltip="{i18n>delete}"
  ariaLabel="{i18n>deleteItem}"/>

<!-- Required field -->
<Label text="Name" required="true" labelFor="nameInput"/>
<Input id="nameInput" value="{name}" required="true"/>
```

## Common UI Patterns

### Empty States
```xml
<IllustratedMessage
  illustrationType="sapIllus-NoData"
  title="{i18n>noProductsTitle}"
  description="{i18n>noProductsDesc}">
  <additionalContent>
    <Button text="{i18n>createFirst}" type="Emphasized" press=".onCreate"/>
  </additionalContent>
</IllustratedMessage>
```

### Loading States
```xml
<!-- Table loading -->
<Table busy="{viewModel>/busy}" busyIndicatorDelay="0">

<!-- Page loading -->
<f:DynamicPage busy="{viewModel>/busy}">

<!-- Skeleton loading (placeholder) -->
<GenericTile state="Loading"/>
```

### Search with Suggestions
```xml
<SearchField
  search=".onSearch"
  suggest=".onSuggest"
  suggestionItems="{/Suggestions}">
  <SuggestionItem text="{name}" description="{category}"/>
</SearchField>
```

### Filter Bar Best Practices
```
Filters to include (prioritize):
1. Most-used filter (e.g., Status)
2. Date range (if applicable)
3. Key dimension (e.g., Category)
4. Free-text search

Max 5-7 visible filters; use "Adapt Filters" for rest
```

## Design Tokens Reference

### Core Colors
```css
--sapBrandColor: #0a6ed1;           /* Primary brand */
--sapHighlightColor: #0854a0;       /* Hover states */
--sapBaseColor: #fff;               /* Card/tile background */
--sapShellColor: #354a5f;           /* Shell header */
--sapBackgroundColor: #f7f7f7;      /* Page background */
```

### Semantic Object Status
| Status | Color | Icon | Use |
|--------|-------|------|-----|
| Positive | Green | ✓ | Success, Active, Complete |
| Negative | Red | ✗ | Error, Blocked, Failed |
| Critical | Orange | ⚠ | Warning, Pending, Attention |
| Information | Blue | ℹ | Info, In Progress |
| Neutral | Grey | — | Default, Draft, Inactive |

## Figma to Fiori Mapping

When analyzing Figma designs, map visual elements to Fiori patterns:

| Figma Element | Fiori Pattern | Control/Annotation |
|---------------|---------------|-------------------|
| Header with back arrow | DynamicPage header | `<f:DynamicPageTitle>` |
| Tab navigation | IconTabBar | `<IconTabBar>` |
| Data table | Responsive Table | `<Table>` / `@UI.LineItem` |
| Form layout | SimpleForm | `<form:SimpleForm>` |
| Card grid | GridList | `<f:GridList>` |
| KPI tile | NumericContent | `<GenericTile>` |
| Pie/Bar chart | VizFrame | `<viz:VizFrame>` |
| Dropdown | Select/ComboBox | `@Common.ValueList` |
| Toggle | Switch | Boolean property |
| Date picker | DatePicker | Date/DateTime type |
| Multi-select | MultiComboBox | Collection |
| Search field | SearchField | `@UI.SelectionFields` |
| Status badge | ObjectStatus | `@UI.Criticality` |
| Avatar | Avatar | - |
| Icon | Icon | sap-icon:// |

## Design Review Checklist

Before finalizing generated code, verify:

- [ ] **Floorplan** - Correct pattern for use case
- [ ] **Hierarchy** - Clear visual hierarchy (titles, sections)
- [ ] **Actions** - Primary action emphasized, proper placement
- [ ] **Feedback** - Loading, empty, error states handled
- [ ] **Responsive** - Works on phone, tablet, desktop
- [ ] **Accessibility** - Labels, keyboard nav, contrast
- [ ] **Consistency** - Follows Fiori design language
- [ ] **Performance** - Pagination/virtualization for large data
- [ ] **i18n** - All text externalized
